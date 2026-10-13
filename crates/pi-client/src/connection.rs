//! Connection lifecycle: transport creation, the version handshake, and inbound messages.

use std::sync::Arc;

use pi_chord::Json;
use pi_chord::services::{parse_wire_service_provider_update, parse_wire_service_subscription_snapshot};
use pi_protocol::{
    ClientHello, ClientMessage, FrameDecoderOptions, ResponseEnvelope, ServerHello, ServerMessage,
    ServerMessageDecoder, encode_client_message,
};
use tokio::sync::oneshot;

use crate::errors::ClientError;
use crate::state::{ActiveService, Effect, Inner, Lifecycle, Reply, State, lock};
use crate::transport::ByteTransportHandlers;
use crate::types::{ConnectionState, ConnectionStateChange};

impl Inner {
    pub(crate) fn frame_options(&self) -> FrameDecoderOptions {
        FrameDecoderOptions::with_max_frame_length(self.max_frame_length)
    }

    pub(crate) async fn connect(&self) -> Result<ServerHello, ClientError> {
        let (sender, receiver) = oneshot::channel();
        let id = self.update(|state, effects| {
            if state.disposed {
                return Err(ClientError::Disposed);
            }
            state.hello = None;
            if !matches!(state.lifecycle, Lifecycle::Disconnected) {
                let current = state.lifecycle.state();
                return Err(ClientError::disconnected_with(format!("Client is already {current}")));
            }
            state.sequence += 1;
            state.lifecycle = Lifecycle::Connecting {
                id: state.sequence,
                decoder: ServerMessageDecoder::new(self.frame_options()),
                handshake: Some(sender),
                transport: None,
            };
            effects
                .push(Effect::StateChange(ConnectionStateChange { state: ConnectionState::Connecting, error: None }));
            Ok(state.sequence)
        })?;
        let opening = (self.factory)(self.handlers(id));
        let me = self.me();
        tokio::spawn(async move { me.open_transport(id, opening).await });
        receiver.await.unwrap_or_else(|_| Err(ClientError::disconnected()))
    }

    fn handlers(&self, id: u64) -> ByteTransportHandlers {
        let (data, close, error) = (self.me.clone(), self.me.clone(), self.me.clone());
        ByteTransportHandlers::new(
            move |chunk| {
                if let Some(inner) = data.upgrade() {
                    inner.handle_data(id, chunk);
                }
            },
            move || {
                if let Some(inner) = close.upgrade() {
                    inner.handle_close(id);
                }
            },
            move |failure| {
                if let Some(inner) = error.upgrade() {
                    inner.update(|state, effects| {
                        if state.is_current(id) {
                            state.fail_and_close(failure.to_disconnected(), effects);
                        }
                    });
                }
            },
        )
    }

    async fn open_transport(
        &self,
        id: u64,
        opening: pi_chord::BoxFuture<Result<Arc<dyn crate::transport::ByteTransport>, ClientError>>,
    ) {
        let transport = match opening.await {
            Ok(transport) => transport,
            Err(error) => {
                self.update(|state, effects| {
                    if state.is_current(id) {
                        state.fail(error.to_disconnected(), effects);
                    }
                });
                return;
            }
        };
        let installed = {
            let mut state = lock(&self.state);
            match &mut state.lifecycle {
                Lifecycle::Connecting { id: current, transport: slot, .. } if *current == id => {
                    *slot = Some(transport.clone());
                    true
                }
                _ => false,
            }
        };
        if !installed {
            transport.close();
            return;
        }
        let hello = encode_client_message(&ClientMessage::Hello(ClientHello::default()), &self.frame_options());
        let result = match hello {
            Ok(frame) => transport.send(frame).await,
            Err(error) => Err(error.into()),
        };
        if let Err(error) = result {
            self.update(|state, effects| {
                if state.is_current(id) {
                    state.fail_and_close(error.to_disconnected(), effects);
                }
            });
        }
    }

    fn handle_data(&self, id: u64, chunk: &[u8]) {
        self.update(|state, effects| {
            let decoded = match &mut state.lifecycle {
                Lifecycle::Connecting { id: current, .. } | Lifecycle::Connected { id: current, .. }
                    if *current != id =>
                {
                    return;
                }
                Lifecycle::Disconnected => return,
                Lifecycle::Connecting { transport: None, .. } => {
                    let error = ClientError::validation("Received server data before the client hello was sent");
                    return state.fail_and_close(error, effects);
                }
                Lifecycle::Connecting { decoder, .. } | Lifecycle::Connected { decoder, .. } => decoder.push(chunk),
            };
            let messages = match decoded {
                Ok(messages) => messages,
                Err(error) => return state.fail_and_close(error.into(), effects),
            };
            for message in messages {
                if matches!(state.lifecycle, Lifecycle::Disconnected) {
                    return;
                }
                self.handle_message(state, message, effects);
            }
        });
    }

    fn handle_message(&self, state: &mut State, message: ServerMessage, effects: &mut Vec<Effect>) {
        if matches!(state.lifecycle, Lifecycle::Connecting { .. }) {
            return self.handle_handshake(state, message, effects);
        }
        match message {
            ServerMessage::Hello(_) | ServerMessage::HelloError(_) => {
                state.fail_and_close(ClientError::validation("Unexpected handshake message"), effects);
            }
            ServerMessage::Attachment(envelope) => {
                if envelope.attachment.as_ref().is_some_and(|target| target.server_id != self.server_id) {
                    let error = ClientError::validation("Attachment update belongs to another server");
                    return state.fail_and_close(error, effects);
                }
                state.set_attachment(envelope.attachment, effects);
            }
            ServerMessage::ServiceUpdate(event) => {
                let Some(active) = state.services.get(&event.subscription_id).cloned() else { return };
                if let Err(error) = queue_update(&active, event.update, effects) {
                    state.fail_and_close(error, effects);
                }
            }
            ServerMessage::Response(response) => self.handle_response(state, response, effects),
        }
    }

    fn handle_handshake(&self, state: &mut State, message: ServerMessage, effects: &mut Vec<Effect>) {
        let hello = match message {
            ServerMessage::HelloError(error) => {
                return state.fail_and_close(ClientError::server(&error.error), effects);
            }
            ServerMessage::Hello(hello) => hello,
            _ => {
                let error = ClientError::validation("Expected server hello as first message");
                return state.fail_and_close(error, effects);
            }
        };
        if hello.server_id != self.server_id {
            let message = format!("Connected server \"{}\" does not match \"{}\"", hello.server_id, self.server_id);
            return state.fail_and_close(ClientError::validation(message), effects);
        }
        let Lifecycle::Connecting { id, decoder, handshake, transport: Some(transport) } =
            std::mem::replace(&mut state.lifecycle, Lifecycle::Disconnected)
        else {
            unreachable!("handshakes run while connecting with a transport");
        };
        state.lifecycle = Lifecycle::Connected { id, decoder, transport };
        state.hello = Some(hello.clone());
        effects.push(Effect::StateChange(ConnectionStateChange { state: ConnectionState::Connected, error: None }));
        if let Some(handshake) = handshake {
            effects.push(Effect::Handshake(handshake, Ok(hello)));
        }
    }

    fn handle_response(&self, state: &mut State, response: ResponseEnvelope, effects: &mut Vec<Effect>) {
        let Some(pending) = state.pending.remove(&response.id) else {
            return state.fail_and_close(ClientError::validation("Response has no matching request"), effects);
        };
        if let Some((signal, id)) = pending.abort {
            effects.push(Effect::RemoveAbortListener(signal, id));
        }
        let reply = match (response.result, &pending.subscription) {
            (Err(error), _) => Err(ClientError::server(&error)),
            (Ok(result), None) => Ok(Reply::Value(result)),
            (Ok(result), Some(active)) => match hydrate(active, result.unwrap_or(Json::Null)) {
                Ok(reply) => Ok(reply),
                Err(error) => {
                    state.fail_and_close(error.clone(), effects);
                    Err(error)
                }
            },
        };
        if let Some(sender) = pending.sender {
            effects.push(Effect::Reply(sender, reply));
        }
    }

    fn handle_close(&self, id: u64) {
        self.update(|state, effects| {
            if !state.is_current(id) {
                return;
            }
            let ended = match &mut state.lifecycle {
                Lifecycle::Connecting { decoder, .. } | Lifecycle::Connected { decoder, .. } => decoder.end(),
                Lifecycle::Disconnected => Ok(()),
            };
            let error = match ended {
                Ok(()) => ClientError::disconnected_with("Byte transport closed"),
                Err(error) => error.into(),
            };
            state.fail(error, effects);
        });
    }
}

fn invalid_stream(error: impl std::fmt::Display) -> ClientError {
    ClientError::validation(error.to_string())
}

/// Install the snapshot, then decode updates that arrived before it.
fn hydrate(active: &ActiveService, result: Json) -> Result<Reply, ClientError> {
    let mut inner = lock(&active.inner);
    let decoder = inner.decoder.as_mut().ok_or_else(|| invalid_stream("Service decoder is missing"))?;
    let wire = parse_wire_service_subscription_snapshot(&result).map_err(invalid_stream)?;
    let snapshot = decoder.decode_snapshot(&wire).map_err(invalid_stream)?;
    inner.hydrated = true;
    for update in std::mem::take(&mut inner.queued_wire) {
        let wire = parse_wire_service_provider_update(&update).map_err(invalid_stream)?;
        let decoder = inner.decoder.as_mut().ok_or_else(|| invalid_stream("Service decoder is missing"))?;
        let decoded = decoder.decode_update(&wire).map_err(invalid_stream)?;
        inner.queued.push(decoded);
    }
    Ok(Reply::Snapshot(snapshot))
}

fn queue_update(active: &Arc<ActiveService>, update: Json, effects: &mut Vec<Effect>) -> Result<(), ClientError> {
    let mut inner = lock(&active.inner);
    if !inner.hydrated {
        inner.queued_wire.push(update);
        return Ok(());
    }
    let wire = parse_wire_service_provider_update(&update).map_err(invalid_stream)?;
    let decoder = inner.decoder.as_mut().ok_or_else(|| invalid_stream("Service decoder is missing"))?;
    let decoded = decoder.decode_update(&wire).map_err(invalid_stream)?;
    if inner.ready {
        inner.delivery.push_back(decoded);
        effects.push(Effect::Deliver(active.clone()));
    } else {
        inner.queued.push(decoded);
    }
    Ok(())
}
