//! Routes presentation connections to hosted Sessions and owns their attachments.

mod attach;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use pi_chord::services::{ServiceCall, ServiceUpdatePublisher};
use pi_chord::{BoxFuture, ChordError, ChordResult, Context, Json};
use pi_protocol::{RpcTarget, SessionTarget};
use tokio::sync::oneshot;

use crate::errors::ServerError;
use crate::task::{SharedTask, collect_errors, lock, spawn_shared};
use crate::types::{ErrorReporter, PresentationOps, RoutedSessionAttachment, RoutedSessionHandle, ServerHost};

/// Sends one connection's out-of-band attachment change.
pub(crate) type PublishAttachment = Arc<dyn Fn(u64, Option<SessionTarget>) -> BoxFuture<()> + Send + Sync>;

type LeaseTask = SharedTask<ChordResult<Arc<dyn RoutedSessionAttachment>>>;
type OpeningTask = SharedTask<ChordResult<Arc<HostedSession>>>;

pub(crate) const DRAINING_CLEANUP_MESSAGE: &str = "Failed to close routed Session acquired while draining";

pub(crate) struct ClientAttachment {
    id: String,
    client: u64,
    session: Arc<HostedSession>,
    operations: Mutex<HashMap<u64, SharedTask<ChordResult<Option<Json>>>>>,
    acquiring: Mutex<Option<LeaseTask>>,
    lease: Mutex<Option<Arc<dyn RoutedSessionAttachment>>>,
    releasing: Mutex<Option<SharedTask<ChordResult<()>>>>,
}

pub(crate) struct HostedSession {
    id: String,
    handle: Arc<dyn RoutedSessionHandle>,
    attachments: Mutex<Vec<Arc<ClientAttachment>>>,
}

#[derive(Default)]
struct RouterState {
    hosted: HashMap<String, Arc<HostedSession>>,
    opening: HashMap<String, (u64, OpeningTask)>,
    by_client: HashMap<u64, Arc<ClientAttachment>>,
    disconnected: HashSet<u64>,
    client_ops: HashMap<u64, (u64, SharedTask<()>)>,
    next_token: u64,
}

pub(crate) struct RouterOptions<H> {
    pub host: Arc<H>,
    pub server_id: String,
    pub closing: Arc<AtomicBool>,
    pub publish: PublishAttachment,
    pub report: ErrorReporter,
}

pub(crate) struct SessionRouter<H: ServerHost> {
    options: RouterOptions<H>,
    state: Mutex<RouterState>,
    close_task: Mutex<Option<SharedTask<ChordResult<()>>>>,
    me: Weak<SessionRouter<H>>,
}

impl<H: ServerHost> SessionRouter<H> {
    pub(crate) fn new(options: RouterOptions<H>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            options,
            state: Mutex::new(RouterState::default()),
            close_task: Mutex::new(None),
            me: me.clone(),
        })
    }

    fn me(&self) -> Arc<Self> {
        self.me.upgrade().unwrap_or_else(|| unreachable!("router methods run on a live Arc"))
    }

    fn closing(&self) -> bool {
        self.options.closing.load(Ordering::SeqCst)
    }

    fn report(&self, error: &ChordError) {
        (self.options.report)(error)
    }

    /// Admit one Session service call in client order; the call itself runs concurrently.
    pub(crate) async fn execute_service_call(
        &self,
        call: ServiceCall,
        target: RpcTarget,
        client: u64,
        publish: ServiceUpdatePublisher,
        context: Context,
    ) -> ChordResult<Option<Json>> {
        let me = self.me();
        let admitted =
            self.run_for_client(client, async move { me.start_service_call(client, &target, call, publish, context) });
        admitted.await?.await
    }

    pub(crate) async fn attach_client(&self, client: u64, session_id: String, context: Context) -> ChordResult<()> {
        if self.closing() {
            return Err(ServerError::server_draining().into());
        }
        let me = self.me();
        self.run_for_client(client, async move { me.attach_client_now(client, session_id, context).await }).await
    }

    pub(crate) async fn detach_client(&self, client: u64, context: Context) -> ChordResult<()> {
        let me = self.me();
        self.run_for_client(client, async move {
            let attachment = lock(&me.state).by_client.get(&client).cloned();
            match attachment {
                Some(attachment) => me.release_attachment(&attachment, context, true).await,
                None => Ok(()),
            }
        })
        .await
    }

    pub(crate) async fn remove_session(&self, session_id: String, context: Context) -> ChordResult<()> {
        if self.closing() {
            return Err(ServerError::server_draining().into());
        }
        let Some(hosted) = lock(&self.state).hosted.get(&session_id).cloned() else { return Ok(()) };
        let attachments = lock(&hosted.attachments).clone();
        let releases = attachments.iter().map(|attachment| self.release_attachment(attachment, context.clone(), true));
        let mut errors: Vec<ChordError> =
            futures::future::join_all(releases).await.into_iter().filter_map(Result::err).collect();
        if let Err(error) = hosted.handle.close(&context).await {
            errors.push(error);
        }
        let mut state = lock(&self.state);
        if state.hosted.get(&session_id).is_some_and(|current| Arc::ptr_eq(current, &hosted)) {
            state.hosted.remove(&session_id);
        }
        drop(state);
        collect_errors(errors, &format!("Failed to close Session {session_id}"))
    }

    /// Release the client's attachment after its admitted calls settle, without publishing.
    pub(crate) async fn disconnect(&self, client: u64, context: Context) -> ChordResult<()> {
        lock(&self.state).disconnected.insert(client);
        let me = self.me();
        let result = self
            .run_for_client(client, async move {
                let attachment = lock(&me.state).by_client.get(&client).cloned();
                match attachment {
                    Some(attachment) => me.release_attachment(&attachment, context, false).await,
                    None => Ok(()),
                }
            })
            .await;
        lock(&self.state).disconnected.remove(&client);
        result
    }

    pub(crate) fn close(&self, context: Context) -> SharedTask<ChordResult<()>> {
        let mut task = lock(&self.close_task);
        let me = self.me();
        task.get_or_insert_with(|| spawn_shared(async move { me.close_internal(context).await })).clone()
    }

    async fn close_internal(&self, context: Context) -> ChordResult<()> {
        let (operations, openings) = {
            let state = lock(&self.state);
            let operations: Vec<_> = state.client_ops.values().map(|(_, task)| task.clone()).collect();
            let openings: Vec<_> = state.opening.values().map(|(_, task)| task.clone()).collect();
            (operations, openings)
        };
        futures::future::join_all(operations).await;
        let mut errors = Vec::new();
        for result in futures::future::join_all(openings).await {
            let Err(error) = result else { continue };
            self.report(&error);
            if matches!(&error, ChordError::Aggregate { message, .. } if message == DRAINING_CLEANUP_MESSAGE) {
                errors.push(error);
            }
        }
        let hosted: Vec<Arc<HostedSession>> = lock(&self.state).hosted.values().cloned().collect();
        let mut releases = Vec::new();
        for session in &hosted {
            for attachment in lock(&session.attachments).clone() {
                releases.push(self.release_attachment(&attachment, context.clone(), true));
            }
        }
        errors.extend(futures::future::join_all(releases).await.into_iter().filter_map(Result::err));
        let closes = hosted.iter().map(|session| session.handle.close(&context));
        for (session, result) in hosted.iter().zip(futures::future::join_all(closes).await) {
            match result {
                Ok(()) => {
                    let mut state = lock(&self.state);
                    if state.hosted.get(&session.id).is_some_and(|current| Arc::ptr_eq(current, session)) {
                        state.hosted.remove(&session.id);
                    }
                }
                Err(error) => {
                    self.report(&error);
                    errors.push(error);
                }
            }
        }
        let mut state = lock(&self.state);
        state.by_client.clear();
        state.client_ops.clear();
        drop(state);
        if errors.is_empty() {
            return Ok(());
        }
        Err(ChordError::Aggregate { message: "Failed to close routed Sessions".into(), errors })
    }

    /// Run `operation` after every earlier operation for the same client, in call order.
    fn run_for_client<T, F>(&self, client: u64, operation: F) -> BoxFuture<ChordResult<T>>
    where
        T: Send + 'static,
        F: std::future::Future<Output = ChordResult<T>> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let me = self.me();
        let mut state = lock(&self.state);
        state.next_token += 1;
        let token = state.next_token;
        let previous = state.client_ops.get(&client).map(|(_, task)| task.clone());
        let task: BoxFuture<()> = Box::pin(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            let _ = sender.send(operation.await);
            let mut state = lock(&me.state);
            if state.client_ops.get(&client).is_some_and(|(current, _)| *current == token) {
                state.client_ops.remove(&client);
            }
        });
        let shared = futures::FutureExt::shared(task);
        state.client_ops.insert(client, (token, shared.clone()));
        drop(state);
        tokio::spawn(shared);
        Box::pin(
            async move { receiver.await.unwrap_or_else(|_| Err(ChordError::error("Client operation was dropped"))) },
        )
    }
}

impl<H: ServerHost> PresentationOps for SessionRouter<H> {
    fn attach(&self, client: u64, session_id: String, context: Context) -> BoxFuture<ChordResult<()>> {
        let me = self.me();
        Box::pin(async move { me.attach_client(client, session_id, context).await })
    }

    fn detach(&self, client: u64, context: Context) -> BoxFuture<ChordResult<()>> {
        let me = self.me();
        Box::pin(async move { me.detach_client(client, context).await })
    }

    fn remove(&self, session_id: String, context: Context) -> BoxFuture<ChordResult<()>> {
        let me = self.me();
        Box::pin(async move { me.remove_session(session_id, context).await })
    }
}
