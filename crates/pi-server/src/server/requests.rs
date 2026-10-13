//! Handshake completion, request routing, cancellation, and service-update delivery.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use pi_chord::services::{
    ServiceCall, ServiceControlCall, ServiceProviderUpdate, ServiceUpdatePublisher, create_service_state_encoder,
    decode_service_control_call, parse_service_call, parse_service_subscription_snapshot,
};
use pi_chord::{AbortController, ChordError, ChordResult, Context, Json, TODO_CONTEXT, with_abort_signal};
use pi_protocol::{
    CancelEnvelope, ClientHello, ClientMessage, PROTOCOL_VERSION, ProtocolError, RequestEnvelope, ResponseEnvelope,
    ServerHello, ServerMessage, ServiceEventEnvelope, SessionTarget, is_supported_protocol_version,
};

use super::ServerInner;
use super::connection::{Conn, Enqueued, Stage};
use crate::errors::{INTERNAL_SERVER_ERROR_MESSAGE, ServerError, find_validation, validation_error};
use crate::task::lock;
use crate::types::{RoutedServerPresentation, ServerHost};

#[derive(Default)]
struct Pending {
    ready: bool,
    updates: VecDeque<ServiceProviderUpdate>,
}

impl<H: ServerHost> ServerInner<H> {
    pub(super) async fn finish_handshake(&self, conn: &Arc<Conn>, hello: ClientHello) -> ChordResult<()> {
        if !is_supported_protocol_version(hello.version) {
            let message = format!("Unsupported protocol version {}; expected {PROTOCOL_VERSION}", hello.version);
            self.fail_protocol(conn, ProtocolError::new("version", message)).await;
            return Ok(());
        }
        if self.handshake_abandoned(conn) {
            return Ok(());
        }
        let presentation = RoutedServerPresentation { client: conn.id, ops: self.sessions.clone() };
        let services = self.host.server_services().attach_client(presentation, &TODO_CONTEXT).await?;
        if self.handshake_abandoned(conn) {
            return services.release(&TODO_CONTEXT).await;
        }
        lock(&conn.state).server_services = Some(services);
        let hello = ServerMessage::Hello(ServerHello { server_id: self.server_id.clone() });
        if self.send_message(conn, hello).await {
            self.mark_ready(conn);
        }
        Ok(())
    }

    fn handshake_abandoned(&self, conn: &Conn) -> bool {
        let state = lock(&conn.state);
        self.closing.load(std::sync::atomic::Ordering::SeqCst)
            || state.disconnected
            || state.stage != Stage::Handshaking
            || conn.connection.closed()
    }

    pub(super) fn dispatch_ready(&self, conn: &Arc<Conn>, message: ClientMessage) {
        match message {
            ClientMessage::Cancel(cancel) => self.handle_cancel(conn, cancel),
            ClientMessage::Request(request) => self.start_request(conn, request),
            ClientMessage::Hello(_) => {}
        }
    }

    fn handle_cancel(&self, conn: &Conn, envelope: CancelEnvelope) {
        if envelope.target.server_id() != self.server_id {
            return;
        }
        let controller = lock(&conn.state)
            .active
            .get(&envelope.id)
            .filter(|(_, target)| *target == envelope.target)
            .map(|(controller, _)| controller.clone());
        if let Some(controller) = controller {
            controller.abort(Some(ChordError::error("RPC request cancelled")));
        }
    }

    fn respond_error(&self, conn: &Arc<Conn>, id: String, message: &str) {
        let (me, conn) = (self.me(), conn.clone());
        let response = ServerMessage::Response(ResponseEnvelope {
            id,
            result: Err(ProtocolError::new("invalid_request", message)),
        });
        tokio::spawn(async move { me.send_message(&conn, response).await });
    }

    /// Register the request synchronously so later cancels find it, then run it.
    fn start_request(&self, conn: &Arc<Conn>, envelope: RequestEnvelope) {
        let mut state = lock(&conn.state);
        if state.active.contains_key(&envelope.id) {
            drop(state);
            return self.respond_error(conn, envelope.id, "Request ID is already active");
        }
        let Ok(call) = parse_service_call(&envelope.call) else {
            drop(state);
            return self.respond_error(conn, envelope.id, "Invalid service call");
        };
        let controller = AbortController::new();
        state.active.insert(envelope.id.clone(), (controller.clone(), envelope.target.clone()));
        drop(state);
        let (me, conn) = (self.me(), conn.clone());
        tokio::spawn(async move { me.handle_request(&conn, envelope, call, controller).await });
    }

    async fn handle_request(
        &self,
        conn: &Arc<Conn>,
        envelope: RequestEnvelope,
        call: ServiceCall,
        controller: AbortController,
    ) {
        let context = with_abort_signal(&controller.signal(), &TODO_CONTEXT);
        let control = decode_service_control_call(&call);
        let subscribing = match &control {
            Some(ServiceControlCall::Subscribe { subscription_id, .. }) => Some(subscription_id.clone()),
            _ => None,
        };
        let pending = Arc::new(Mutex::new(Pending { ready: subscribing.is_none(), ..Pending::default() }));
        let publish = self.publisher(conn, subscribing.clone(), pending.clone());
        let outcome = self.route(conn, &envelope, call, control.as_ref(), publish, context).await;
        let id = envelope.id.clone();
        match outcome {
            Ok(result) => {
                let response = ServerMessage::Response(ResponseEnvelope { id: id.clone(), result: Ok(result) });
                self.send_message(conn, response).await;
                if let Some(subscription_id) = &subscribing
                    && let Err(error) = self.flush_pending(conn, subscription_id, &pending).await
                {
                    self.report_error(&error);
                    self.close_connection(conn, None).await;
                    self.disconnect(conn);
                }
            }
            Err(error) => {
                let error = if controller.signal().aborted() {
                    ProtocolError::new("cancelled", "RPC request cancelled")
                } else {
                    self.to_protocol_error(&error)
                };
                let response = ServerMessage::Response(ResponseEnvelope { id: id.clone(), result: Err(error) });
                self.send_message(conn, response).await;
            }
        }
        let mut state = lock(&conn.state);
        if state.active.get(&id).is_some_and(|(current, _)| current.signal().ptr_eq(&controller.signal())) {
            state.active.remove(&id);
        }
    }

    async fn route(
        &self,
        conn: &Arc<Conn>,
        envelope: &RequestEnvelope,
        call: ServiceCall,
        control: Option<&ServiceControlCall>,
        publish: ServiceUpdatePublisher,
        context: Context,
    ) -> ChordResult<Option<Json>> {
        if envelope.target.server_id() != self.server_id {
            return Err(ServerError::wrong_server().into());
        }
        if let Some(ServiceControlCall::Subscribe { subscription_id, .. }) = control
            && lock(&conn.state).encoders.contains_key(subscription_id)
        {
            return Err(validation_error(format!("Duplicate service subscription {subscription_id}")));
        }
        let services = lock(&conn.state).server_services.clone();
        let result = if envelope.target.session().is_some() {
            let target = envelope.target.clone();
            self.sessions.execute_service_call(call, target, conn.id, publish, context).await?
        } else if let Some(services) = services {
            services.invoke_service(call, publish, &context).await?
        } else {
            return Err(validation_error(format!("Unknown service member {}.{}", call.service_id, call.member)));
        };
        match control {
            Some(ServiceControlCall::Subscribe { subscription_id, .. }) => {
                let value = result.ok_or_else(|| validation_error("Service subscription did not return a snapshot"))?;
                let mut encoder = create_service_state_encoder();
                let wire = encoder.encode_snapshot(&parse_service_subscription_snapshot(&value)?)?;
                lock(&conn.state).encoders.insert(subscription_id.clone(), encoder);
                Ok(Some(wire.to_json()))
            }
            Some(ServiceControlCall::Unsubscribe { subscription_id }) => {
                lock(&conn.state).encoders.remove(subscription_id);
                Ok(result)
            }
            _ => Ok(result),
        }
    }

    /// Hold the subscribing request's updates until its snapshot response is queued.
    fn publisher(
        &self,
        conn: &Arc<Conn>,
        subscribing: Option<String>,
        pending: Arc<Mutex<Pending>>,
    ) -> ServiceUpdatePublisher {
        let (me, conn) = (self.me(), conn.clone());
        Arc::new(move |subscription_id: &str, update: ServiceProviderUpdate, _context: &Context| {
            if subscribing.as_deref() == Some(subscription_id) {
                let mut pending = lock(&pending);
                if !pending.ready {
                    pending.updates.push_back(update);
                    return;
                }
            }
            let enqueued = me.enqueue_update(&conn, subscription_id, update);
            let (server, conn) = (me.clone(), conn.clone());
            tokio::spawn(async move { server.complete(&conn, enqueued).await });
        })
    }

    async fn flush_pending(
        &self,
        conn: &Arc<Conn>,
        subscription_id: &str,
        pending: &Mutex<Pending>,
    ) -> ChordResult<()> {
        loop {
            let update = {
                let mut pending = lock(pending);
                match pending.updates.pop_front() {
                    Some(update) => update,
                    None => {
                        pending.ready = true;
                        return Ok(());
                    }
                }
            };
            match self.enqueue_update(conn, subscription_id, update) {
                Enqueued::Failed(error) => return Err(error),
                enqueued => {
                    self.complete(conn, enqueued).await;
                }
            }
        }
    }

    /// Encode with the subscription's stateful encoder and queue in one critical section.
    fn enqueue_update(&self, conn: &Conn, subscription_id: &str, update: ServiceProviderUpdate) -> Enqueued {
        let mut state = lock(&conn.state);
        let Some(encoder) = state.encoders.get_mut(subscription_id) else { return Enqueued::Skipped };
        let wire = match encoder.encode_update(&update) {
            Ok(wire) => wire,
            Err(error) => return Enqueued::Failed(error),
        };
        let message = ServerMessage::ServiceUpdate(ServiceEventEnvelope {
            subscription_id: subscription_id.to_string(),
            update: wire.to_json(),
        });
        self.enqueue(conn, &state, &message)
    }

    pub(super) async fn publish_attachment(&self, client: u64, attachment: Option<SessionTarget>) {
        let conn = lock(&self.connections).get(&client).cloned();
        if let Some(conn) = conn {
            let message = ServerMessage::Attachment(pi_protocol::AttachmentEnvelope { attachment });
            self.send_message(&conn, message).await;
        }
    }

    pub(super) fn to_protocol_error(&self, error: &ChordError) -> ProtocolError {
        if let ChordError::Remote(remote) = error {
            return ProtocolError::new(remote.code.as_str(), remote.message.clone());
        }
        if let Some(server) = ServerError::find(error) {
            return ProtocolError::new(server.code.as_str(), server.message.clone());
        }
        if let Some(validation) = find_validation(error) {
            return ProtocolError::new("invalid_request", validation.0.clone());
        }
        self.report_error(error);
        ProtocolError::new("internal_error", INTERNAL_SERVER_ERROR_MESSAGE)
    }
}
