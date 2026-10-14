//! The selected-Session remote service source (upstream `SessionServiceSourceImpl` in `services/connection.ts`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use pi_chord::services::{
    RemoteServiceSource, RemoteServiceSourceOpenOptions, RemoteServiceTransport, RemoteServices, ServiceCatalogueEntry,
};
use pi_chord::state::{ErrorReporter, MutableReplicatedState};
use pi_chord::{BACKGROUND_CONTEXT, ChordError, ChordResult, Context};
use pi_client::{Client, ListenerGuard, create_client_service_transport};
use pi_protocol::{RpcTarget, SessionTarget};
use tokio::task::JoinHandle;

use super::codec::encode;
use super::connection::{
    RoutedActivate, RoutedBindings, SessionAttachmentState, attachment_value, dispose_all, open_binding,
    publish_attachment, rebind_all, same_attachment,
};
use crate::experimental::process::lock;

struct Inner {
    client: Client,
    transport: Arc<dyn RemoteServiceTransport>,
    attachment: MutableReplicatedState,
    bindings: RoutedBindings,
    catalogue: Mutex<Option<Vec<ServiceCatalogueEntry>>>,
    revision: AtomicU64,
    transitions: Mutex<Vec<JoinHandle<()>>>,
    on_error: ErrorReporter,
}

/// The selected-Session remote service source for one presentation client.
pub struct SessionServiceSource {
    inner: Arc<Inner>,
    guard: Mutex<Option<ListenerGuard>>,
    disposed: AtomicBool,
}

impl SessionServiceSource {
    pub fn new(client: &Client, on_error: Option<ErrorReporter>) -> ChordResult<Arc<Self>> {
        let target_client = client.clone();
        let transport = create_client_service_transport(
            client,
            Arc::new(move || target_client.attachment().map(RpcTarget::Session)),
        );
        let initial = match client.attachment() {
            None => SessionAttachmentState::Detached,
            Some(target) => SessionAttachmentState::Attaching { session_id: target.session_id },
        };
        let inner = Arc::new(Inner {
            client: client.clone(),
            transport,
            attachment: MutableReplicatedState::new(encode(&initial)?),
            bindings: Arc::default(),
            catalogue: Mutex::default(),
            revision: AtomicU64::new(0),
            transitions: Mutex::default(),
            on_error: on_error.unwrap_or_else(|| Arc::new(|_| {})),
        });
        let weak = Arc::downgrade(&inner);
        let guard = client
            .on_attachment_change(Arc::new(move |attachment| {
                if let Some(inner) = weak.upgrade() {
                    Inner::attachment_changed(&inner, attachment.cloned());
                }
            }))
            .map_err(ChordError::other)?;
        Ok(Arc::new(Self { inner, guard: Mutex::new(Some(guard)), disposed: AtomicBool::new(false) }))
    }

    /// The attachment state, published as replicated state.
    pub fn attachment(&self) -> &MutableReplicatedState {
        &self.inner.attachment
    }

    pub fn attachment_state(&self) -> Option<SessionAttachmentState> {
        attachment_value(&self.inner.attachment)
    }

    /// Wait for the exact current attachment generation to finish hydrating.
    pub async fn when_attached(&self, session_id: &str, context: &Context) -> ChordResult<()> {
        let attachment = self.inner.client.attachment().filter(|target| target.session_id == session_id);
        let Some(attachment) = attachment else {
            return Err(ChordError::error(format!("Session {session_id} is not the current attachment")));
        };
        self.inner.when_attached(&attachment, self.inner.revision.load(Ordering::SeqCst), context).await
    }

    /// Wait for every binding to finish releasing the earlier attachment.
    pub async fn when_detached(&self, context: &Context) -> ChordResult<()> {
        if self.inner.client.attachment().is_some() {
            return Err(ChordError::error("A Session is still attached"));
        }
        self.inner.when_detached(self.inner.revision.load(Ordering::SeqCst), context).await
    }

    pub async fn dispose(&self, context: &Context) -> ChordResult<()> {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(guard) = lock(&self.guard).take() {
            guard.remove();
        }
        let transitions: Vec<JoinHandle<()>> = std::mem::take(&mut *lock(&self.inner.transitions));
        futures::future::join_all(transitions).await;
        dispose_all(&self.inner.bindings, context, "Failed to dispose Session service source").await
    }
}

impl Inner {
    fn current(&self, revision: u64, attachment: Option<&SessionTarget>) -> bool {
        self.revision.load(Ordering::SeqCst) == revision
            && same_attachment(self.client.attachment().as_ref(), attachment)
    }

    fn attachment_changed(inner: &Arc<Self>, attachment: Option<SessionTarget>) {
        let revision = inner.revision.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(target) = &attachment {
            let attaching = SessionAttachmentState::Attaching { session_id: target.session_id.clone() };
            publish_attachment(&inner.attachment, &attaching, &BACKGROUND_CONTEXT);
            let (this, target) = (inner.clone(), target.clone());
            tokio::spawn(async move {
                match this.client.service_catalogue(RpcTarget::Session(target.clone()), None).await {
                    Ok(catalogue) if this.current(revision, Some(&target)) => *lock(&this.catalogue) = Some(catalogue),
                    Ok(_) => {}
                    Err(error) => (this.on_error)(ChordError::other(error)),
                }
            });
        }
        let this = inner.clone();
        let task = tokio::spawn(async move {
            let bound = attachment.is_some();
            let result =
                rebind_all(&this.bindings, bound, &BACKGROUND_CONTEXT, "Failed to rebind Session services").await;
            if !this.current(revision, attachment.as_ref()) {
                return;
            }
            let state = match (&attachment, &result) {
                (None, _) => SessionAttachmentState::Detached,
                (Some(target), Ok(())) => SessionAttachmentState::Attached { session_id: target.session_id.clone() },
                (Some(target), Err(_)) => SessionAttachmentState::Degraded { session_id: target.session_id.clone() },
            };
            publish_attachment(&this.attachment, &state, &BACKGROUND_CONTEXT);
            if let Err(error) = result {
                (this.on_error)(error);
            }
        });
        let mut transitions = lock(&inner.transitions);
        transitions.retain(|task| !task.is_finished());
        transitions.push(task);
    }

    async fn bindings_ready(&self, context: &Context) -> ChordResult<()> {
        let bindings = lock(&self.bindings).clone();
        futures::future::try_join_all(bindings.iter().map(|binding| binding.ready_service(context))).await.map(drop)
    }

    async fn when_attached(&self, attachment: &SessionTarget, revision: u64, context: &Context) -> ChordResult<()> {
        if let Err(error) = self.bindings_ready(context).await {
            if self.current(revision, Some(attachment)) {
                let degraded = SessionAttachmentState::Degraded { session_id: attachment.session_id.clone() };
                publish_attachment(&self.attachment, &degraded, context);
            }
            return Err(error);
        }
        if !self.current(revision, Some(attachment)) {
            return Err(ChordError::error(format!("Session {} was replaced while attaching", attachment.session_id)));
        }
        let attached = SessionAttachmentState::Attached { session_id: attachment.session_id.clone() };
        if attachment_value(&self.attachment).as_ref() != Some(&attached) {
            publish_attachment(&self.attachment, &attached, context);
        }
        Ok(())
    }

    async fn when_detached(&self, revision: u64, context: &Context) -> ChordResult<()> {
        self.bindings_ready(context).await?;
        if self.revision.load(Ordering::SeqCst) != revision || self.client.attachment().is_some() {
            return Err(ChordError::error("The Session attachment changed while detaching"));
        }
        if attachment_value(&self.attachment) != Some(SessionAttachmentState::Detached) {
            publish_attachment(&self.attachment, &SessionAttachmentState::Detached, context);
        }
        Ok(())
    }
}

#[async_trait]
impl RemoteServiceSource for SessionServiceSource {
    fn accepts_unavailable_services(&self) -> bool {
        self.inner.client.attachment().is_none() && lock(&self.inner.catalogue).is_none()
    }

    async fn catalogue(&self, context: &Context) -> ChordResult<Vec<ServiceCatalogueEntry>> {
        let Some(target) = self.inner.client.attachment() else {
            return Ok(lock(&self.inner.catalogue).clone().unwrap_or_default());
        };
        let catalogue = self.inner.client.service_catalogue(RpcTarget::Session(target), context.abort_signal()).await;
        let catalogue = catalogue.map_err(ChordError::other)?;
        *lock(&self.inner.catalogue) = Some(catalogue.clone());
        Ok(catalogue)
    }

    fn open(&self, options: RemoteServiceSourceOpenOptions) -> ChordResult<Arc<dyn RemoteServices>> {
        if self.disposed.load(Ordering::SeqCst) {
            return Err(ChordError::error("Session service source is disposed"));
        }
        let client = self.inner.client.clone();
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        let activate: RoutedActivate = Arc::new(move |context| {
            let weak = weak.clone();
            Box::pin(async move {
                let Some(inner) = weak.upgrade() else { return Ok(()) };
                let revision = inner.revision.load(Ordering::SeqCst);
                match inner.client.attachment() {
                    None => inner.when_detached(revision, &context).await,
                    Some(attachment) => inner.when_attached(&attachment, revision, &context).await,
                }
            })
        });
        let get_bound = Box::new(move || client.attachment().is_some());
        let binding = open_binding(&self.inner.bindings, &self.inner.transport, options, get_bound, Some(activate))?;
        Ok(binding)
    }
}
