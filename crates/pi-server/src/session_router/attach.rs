//! Acquiring, using, and releasing one presentation's Session attachment.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::services::{ServiceCall, ServiceUpdatePublisher};
use pi_chord::{ChordError, ChordResult, Context, Json};
use pi_protocol::{RpcTarget, SessionTarget};

use super::{ClientAttachment, DRAINING_CLEANUP_MESSAGE, HostedSession, SessionRouter};
use crate::errors::ServerError;
use crate::task::{SharedTask, collect_errors, lock, spawn_shared};
use crate::types::{ServerHost, SessionMetadata};
use crate::uuid::random_uuid;

static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);

fn draining() -> ChordError {
    ServerError::server_draining().into()
}

impl<H: ServerHost> SessionRouter<H> {
    fn is_disconnected(&self, client: u64) -> bool {
        lock(&self.state).disconnected.contains(&client)
    }

    pub(super) async fn attach_client_now(&self, client: u64, session_id: String, context: Context) -> ChordResult<()> {
        if self.closing() || self.is_disconnected(client) {
            return Err(draining());
        }
        let current = lock(&self.state).by_client.get(&client).cloned();
        if current.as_ref().is_some_and(|current| current.session.id == session_id) {
            return Ok(());
        }
        let hosted = self.acquire(&session_id, &context).await?;
        if self.closing() || self.is_disconnected(client) {
            return Err(draining());
        }
        if let Some(current) = current {
            self.release_attachment(&current, context.clone(), false).await?;
        }
        let attachment = Arc::new(ClientAttachment {
            id: random_uuid(),
            client,
            session: hosted.clone(),
            operations: Mutex::new(HashMap::new()),
            acquiring: Mutex::new(None),
            lease: Mutex::new(None),
            releasing: Mutex::new(None),
        });
        lock(&hosted.attachments).push(attachment.clone());
        let handle = hosted.handle.clone();
        let acquire_context = context.clone();
        let acquiring = spawn_shared(async move { handle.attach_client(&acquire_context).await });
        *lock(&attachment.acquiring) = Some(acquiring.clone());
        match acquiring.await {
            Ok(lease) => *lock(&attachment.lease) = Some(lease),
            Err(error) => {
                lock(&hosted.attachments).retain(|candidate| !Arc::ptr_eq(candidate, &attachment));
                return Err(error);
            }
        }
        let still_hosted =
            lock(&self.state).hosted.get(&hosted.id).is_some_and(|current| Arc::ptr_eq(current, &hosted));
        let still_attached = lock(&hosted.attachments).iter().any(|candidate| Arc::ptr_eq(candidate, &attachment));
        if !still_hosted || !still_attached || self.is_disconnected(client) || self.closing() {
            self.release_attachment(&attachment, context, true).await?;
            return Err(draining());
        }
        lock(&self.state).by_client.insert(client, attachment.clone());
        let target = SessionTarget {
            server_id: self.options.server_id.clone(),
            session_id,
            attachment_id: attachment.id.clone(),
        };
        (self.options.publish)(client, Some(target)).await;
        Ok(())
    }

    pub(super) fn start_service_call(
        &self,
        client: u64,
        target: &RpcTarget,
        call: ServiceCall,
        publish: ServiceUpdatePublisher,
        context: Context,
    ) -> ChordResult<SharedTask<ChordResult<Option<Json>>>> {
        let attachment = self.require_attachment(client, target)?;
        let lease =
            lock(&attachment.lease).clone().ok_or_else(|| ChordError::from(ServerError::session_not_attached()))?;
        let result = spawn_shared(async move { lease.invoke_service(call, publish, &context).await });
        let operation = NEXT_OPERATION.fetch_add(1, Ordering::Relaxed);
        lock(&attachment.operations).insert(operation, result.clone());
        let watched = result.clone();
        tokio::spawn(async move {
            let _ = watched.await;
            lock(&attachment.operations).remove(&operation);
        });
        Ok(result)
    }

    fn require_attachment(&self, client: u64, target: &RpcTarget) -> ChordResult<Arc<ClientAttachment>> {
        if self.closing() || self.is_disconnected(client) {
            return Err(draining());
        }
        let not_attached = || ChordError::from(ServerError::session_not_attached());
        let target = target.session().ok_or_else(not_attached)?;
        let attachment = lock(&self.state).by_client.get(&client).cloned().ok_or_else(not_attached)?;
        if attachment.session.id != target.session_id || attachment.id != target.attachment_id {
            return Err(not_attached());
        }
        Ok(attachment)
    }

    /// Release once, after admitted calls settle. Every caller shares the same outcome.
    pub(super) fn release_attachment(
        &self,
        attachment: &Arc<ClientAttachment>,
        context: Context,
        publish: bool,
    ) -> SharedTask<ChordResult<()>> {
        let mut releasing = lock(&attachment.releasing);
        if let Some(task) = releasing.as_ref() {
            return task.clone();
        }
        let me = self.me();
        let attachment = attachment.clone();
        let task = spawn_shared(async move {
            let operations: Vec<_> = lock(&attachment.operations).values().cloned().collect();
            futures::future::join_all(operations).await;
            let mut errors = Vec::new();
            let lease = lock(&attachment.lease).clone();
            let lease = match lease {
                Some(lease) => Some(lease),
                None => {
                    let acquiring = lock(&attachment.acquiring).clone();
                    match acquiring {
                        Some(acquiring) => acquiring.await.map_err(|error| errors.push(error)).ok(),
                        None => None,
                    }
                }
            };
            if let Some(lease) = lease
                && let Err(error) = lease.release(&context).await
            {
                errors.push(error);
            }
            me.clear_attachment(&attachment, publish).await;
            collect_errors(errors, "Failed to release Session attachment")
        });
        *releasing = Some(task.clone());
        task
    }

    async fn clear_attachment(&self, attachment: &Arc<ClientAttachment>, publish: bool) {
        lock(&attachment.session.attachments).retain(|candidate| !Arc::ptr_eq(candidate, attachment));
        let owned = {
            let mut state = lock(&self.state);
            let owned = state.by_client.get(&attachment.client).is_some_and(|current| Arc::ptr_eq(current, attachment));
            if owned {
                state.by_client.remove(&attachment.client);
            }
            owned
        };
        if owned && publish {
            (self.options.publish)(attachment.client, None).await;
        }
    }

    async fn acquire(&self, session_id: &str, context: &Context) -> ChordResult<Arc<HostedSession>> {
        let (token, task) = {
            let mut state = lock(&self.state);
            if let Some(existing) = state.hosted.get(session_id) {
                return Ok(existing.clone());
            }
            match state.opening.get(session_id) {
                Some((token, task)) => (*token, task.clone()),
                None => {
                    let me = self.me();
                    let (id, context) = (session_id.to_string(), context.clone());
                    let task = spawn_shared(async move { me.open(id, context).await });
                    state.next_token += 1;
                    let token = state.next_token;
                    state.opening.insert(session_id.to_string(), (token, task.clone()));
                    (token, task)
                }
            }
        };
        let result = task.await;
        let mut state = lock(&self.state);
        if state.opening.get(session_id).is_some_and(|(current, _)| *current == token) {
            state.opening.remove(session_id);
        }
        result
    }

    async fn open(&self, session_id: String, context: Context) -> ChordResult<Arc<HostedSession>> {
        let metadata = self.options.host.resolve_session(&session_id, &context).await?;
        let id = metadata.id().to_string();
        let handle = self.options.host.open_session(metadata, &context).await?;
        if self.closing() {
            if let Err(error) = handle.close(&context).await {
                self.report(&error);
                return Err(ChordError::Aggregate {
                    message: DRAINING_CLEANUP_MESSAGE.into(),
                    errors: vec![draining(), error],
                });
            }
            return Err(draining());
        }
        let hosted =
            Arc::new(HostedSession { id: id.clone(), handle: handle.clone(), attachments: Mutex::new(vec![]) });
        lock(&self.state).hosted.insert(id, hosted.clone());
        if let Some(terminated) = handle.terminated() {
            let me = self.me();
            let watched = hosted.clone();
            tokio::spawn(async move {
                let error = terminated.await;
                me.invalidate(&watched, error);
            });
        }
        Ok(hosted)
    }

    fn invalidate(&self, hosted: &Arc<HostedSession>, error: Option<ChordError>) {
        {
            let mut state = lock(&self.state);
            if !state.hosted.get(&hosted.id).is_some_and(|current| Arc::ptr_eq(current, hosted)) {
                return;
            }
            state.hosted.remove(&hosted.id);
        }
        for attachment in lock(&hosted.attachments).clone() {
            let release = self.release_attachment(&attachment, Context::background(), true);
            let me = self.me();
            tokio::spawn(async move {
                if let Err(error) = release.await {
                    me.report(&error);
                }
            });
        }
        if let Some(error) = error {
            self.report(&error);
        }
    }
}
