//! Per-connection state: decoding, staged dispatch, sending, and teardown.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pi_chord::services::ServiceStateEncoder;
use pi_chord::{AbortController, BoxFuture, ChordError, ChordResult, Context};
use pi_protocol::{
    ClientMessage, ClientMessageDecoder, FrameDecoderOptions, ProtocolError, RpcTarget, ServerHelloError,
    ServerMessage, encode_server_message,
};
use tokio::task::AbortHandle;

use super::ServerInner;
use crate::connection::{ByteConnection, ByteConnectionHandler};
use crate::task::lock;
use crate::types::{RoutedServerServiceAttachment, ServerHost};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    AwaitingHello,
    Handshaking,
    Ready,
    Closing,
    Closed,
}

pub(crate) struct ConnState {
    pub decoder: ClientMessageDecoder,
    pub encoders: HashMap<String, ServiceStateEncoder>,
    pub stage: Stage,
    pub disconnected: bool,
    pub queued: Vec<ClientMessage>,
    pub timer: Option<AbortHandle>,
    pub server_services: Option<Arc<dyn RoutedServerServiceAttachment>>,
    pub active: HashMap<String, (AbortController, RpcTarget)>,
}

impl ConnState {
    pub fn is_terminal(&self) -> bool {
        self.disconnected || matches!(self.stage, Stage::Closing | Stage::Closed)
    }

    fn clear_timer(&mut self) {
        if let Some(timer) = self.timer.take() {
            timer.abort();
        }
    }
}

pub(crate) struct Conn {
    pub id: u64,
    pub connection: Arc<dyn ByteConnection>,
    pub state: Mutex<ConnState>,
}

impl Conn {
    pub fn begin_closing(&self) {
        let mut state = lock(&self.state);
        state.stage = Stage::Closing;
        state.clear_timer();
    }
}

/// A message queued on a connection, or why it was not.
pub(crate) enum Enqueued {
    Skipped,
    Failed(ChordError),
    Sent(BoxFuture<ChordResult<()>>),
}

impl<H: ServerHost> ServerInner<H> {
    pub(super) fn accept(&self, connection: Arc<dyn ByteConnection>) -> ByteConnectionHandler {
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::spawn(connection.close(None));
            let me = self.me();
            return ByteConnectionHandler::new(|_| {}, || {}, move |error| me.report_error(&error));
        }
        let id = self.next_connection.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let conn = Arc::new(Conn {
            id,
            connection,
            state: Mutex::new(ConnState {
                decoder: ClientMessageDecoder::new(FrameDecoderOptions::with_max_frame_length(self.max_frame_length)),
                encoders: HashMap::new(),
                stage: Stage::AwaitingHello,
                disconnected: false,
                queued: Vec::new(),
                timer: None,
                server_services: None,
                active: HashMap::new(),
            }),
        });
        let (me, timed, timeout) = (self.me(), conn.clone(), self.handshake_timeout);
        let timer = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            me.fail_protocol(&timed, ProtocolError::new("invalid_request", "Handshake timeout")).await;
        });
        lock(&conn.state).timer = Some(timer.abort_handle());
        lock(&self.connections).insert(id, conn.clone());
        self.notify_connection_count_changed();

        let (data, close, error) = (self.me(), self.me(), self.me());
        let (data_conn, close_conn, error_conn) = (conn.clone(), conn.clone(), conn);
        ByteConnectionHandler::new(
            move |chunk| data.receive(&data_conn, chunk),
            move || close.transport_closed(&close_conn),
            move |failure| {
                error.report_error(&failure);
                let (server, conn) = (error.clone(), error_conn.clone());
                tokio::spawn(async move {
                    server.close_connection(&conn, None).await;
                    server.disconnect(&conn);
                });
            },
        )
    }

    fn receive(&self, conn: &Arc<Conn>, chunk: &[u8]) {
        let messages = {
            let mut state = lock(&conn.state);
            if state.is_terminal() {
                return;
            }
            match state.decoder.push(chunk) {
                Ok(messages) => messages,
                Err(error) => {
                    drop(state);
                    self.spawn_fail(conn, ProtocolError::new("invalid_request", error.0));
                    return;
                }
            }
        };
        for message in messages {
            if lock(&conn.state).is_terminal() {
                return;
            }
            self.dispatch(conn, message);
        }
    }

    fn dispatch(&self, conn: &Arc<Conn>, message: ClientMessage) {
        let mut state = lock(&conn.state);
        if state.stage == Stage::AwaitingHello {
            let ClientMessage::Hello(hello) = message else {
                drop(state);
                self.spawn_fail(conn, ProtocolError::new("invalid_request", "The first client message must be hello"));
                return;
            };
            state.stage = Stage::Handshaking;
            drop(state);
            let (me, conn) = (self.me(), conn.clone());
            tokio::spawn(async move {
                if let Err(error) = me.finish_handshake(&conn, hello).await {
                    let error = me.to_protocol_error(&error);
                    me.fail_protocol(&conn, error).await;
                }
            });
            return;
        }
        if matches!(message, ClientMessage::Hello(_)) {
            drop(state);
            self.spawn_fail(conn, ProtocolError::new("invalid_request", "hello may only be sent as the first message"));
            return;
        }
        match state.stage {
            Stage::Ready => {
                drop(state);
                self.dispatch_ready(conn, message);
            }
            Stage::Handshaking => state.queued.push(message),
            _ => {}
        }
    }

    /// Dispatch requests queued during the handshake in order, then admit new ones directly.
    pub(super) fn mark_ready(&self, conn: &Arc<Conn>) {
        loop {
            let message = {
                let mut state = lock(&conn.state);
                if state.disconnected || state.stage != Stage::Handshaking {
                    return;
                }
                if state.queued.is_empty() {
                    state.stage = Stage::Ready;
                    state.clear_timer();
                    return;
                }
                state.queued.remove(0)
            };
            self.dispatch_ready(conn, message);
        }
    }

    fn spawn_fail(&self, conn: &Arc<Conn>, error: ProtocolError) {
        let (me, conn) = (self.me(), conn.clone());
        tokio::spawn(async move { me.fail_protocol(&conn, error).await });
    }

    /// Encode and queue `message` while the caller holds the connection lock.
    pub(super) fn enqueue(&self, conn: &Conn, state: &ConnState, message: &ServerMessage) -> Enqueued {
        if state.disconnected || conn.connection.closed() {
            return Enqueued::Skipped;
        }
        let options = FrameDecoderOptions::with_max_frame_length(self.max_frame_length);
        match encode_server_message(message, &options) {
            Ok(frame) => Enqueued::Sent(conn.connection.send(frame)),
            Err(error) => Enqueued::Failed(ChordError::other(error)),
        }
    }

    /// Await a queued send; failures close the connection.
    pub(super) async fn complete(&self, conn: &Arc<Conn>, enqueued: Enqueued) -> bool {
        let error = match enqueued {
            Enqueued::Skipped => return false,
            Enqueued::Failed(error) => error,
            Enqueued::Sent(sending) => match sending.await {
                Ok(()) => return true,
                Err(error) => error,
            },
        };
        self.report_error(&error);
        self.close_connection(conn, None).await;
        self.disconnect(conn);
        false
    }

    pub(super) async fn send_message(&self, conn: &Arc<Conn>, message: ServerMessage) -> bool {
        let enqueued = {
            let state = lock(&conn.state);
            self.enqueue(conn, &state, &message)
        };
        self.complete(conn, enqueued).await
    }

    pub(super) async fn fail_protocol(&self, conn: &Arc<Conn>, error: ProtocolError) {
        let final_frame = {
            let mut state = lock(&conn.state);
            if state.is_terminal() {
                return;
            }
            state.stage = Stage::Closing;
            state.clear_timer();
            let message = ServerMessage::HelloError(ServerHelloError { error });
            let options = FrameDecoderOptions::with_max_frame_length(self.max_frame_length);
            encode_server_message(&message, &options).map_err(|error| self.report_error(&ChordError::other(error))).ok()
        };
        self.close_connection(conn, final_frame).await;
        self.disconnect(conn);
    }

    pub(super) async fn close_connection(&self, conn: &Conn, final_chunk: Option<Vec<u8>>) {
        if let Err(error) = conn.connection.close(final_chunk).await {
            self.report_error(&error);
        }
    }

    fn transport_closed(&self, conn: &Arc<Conn>) {
        let decoder_error = {
            let mut state = lock(&conn.state);
            if !state.disconnected && state.stage != Stage::Closing { state.decoder.end().err() } else { None }
        };
        if let Some(error) = decoder_error {
            self.report_error(&ChordError::other(error));
        }
        self.disconnect(conn);
    }

    pub(super) fn disconnect(&self, conn: &Arc<Conn>) {
        let (controllers, services) = {
            let mut state = lock(&conn.state);
            if state.disconnected {
                return;
            }
            state.disconnected = true;
            state.stage = Stage::Closed;
            state.clear_timer();
            state.encoders.clear();
            state.queued.clear();
            let controllers: Vec<AbortController> =
                state.active.drain().map(|(_, (controller, _))| controller).collect();
            (controllers, state.server_services.take())
        };
        for controller in controllers {
            controller.abort(Some(ChordError::error("Client disconnected")));
        }
        if lock(&self.connections).remove(&conn.id).is_some() {
            self.notify_connection_count_changed();
        }
        let (me, client) = (self.me(), conn.id);
        tokio::spawn(async move {
            let context = Context::background();
            let release = async {
                match services {
                    Some(services) => services.release(&context).await,
                    None => Ok(()),
                }
            };
            let (left, right) = futures::join!(me.sessions.disconnect(client, context.clone()), release);
            for error in [left.err(), right.err()].into_iter().flatten() {
                me.report_error(&error);
            }
        });
    }
}
