//! Port of upstream `test/experimental-radius-relay.test.ts`. It is its own test binary: the `experimental`
//! tests mark their processes offline, and Radius auth resolution reads that mark in-process.
//! Sockets report events on a channel, so where upstream checks right after an event, these tests wait.
#![cfg(unix)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use pi_chord::{ChordError, ChordResult};
use pi_client::{ByteTransportHandlers, ClientError, ConnectionState};
use pi_coding_agent::experimental::radius_auth::{RadiusAuthInput, RadiusRelayAuthResolver};
use pi_coding_agent::experimental::radius_client::{
    RadiusClientReconnect, RadiusReconnectClient, Remove, create_radius_client_transport_factory,
};
use pi_coding_agent::experimental::radius_relay::{
    RadiusRelayHost, RadiusRelayHostOptions, RadiusRelayWebSocket, RadiusRelayWebSocketFactory,
    RadiusRelayWebSocketRequest, RelayFrame, RelaySocketEvent, encode_relay_data_frame, parse_relay_data_frame,
};
use pi_coding_agent::experimental::relay::{RadiusRelayHostStatus, RelayHost};
use pi_server::{ByteConnection, ByteConnectionHandler};
use serde_json::json;
use tokio::sync::mpsc;

const SERVER_ID: &str = "00000000-0000-4000-8000-000000000001";
const CONNECTION_ID: &str = "00000000-0000-4000-8000-000000000002";

#[derive(Default)]
struct SocketState {
    sent: Vec<RelayFrame>,
    open: bool,
    closed: bool,
}

struct FakeWebSocket {
    state: Mutex<SocketState>,
    events: mpsc::UnboundedSender<RelaySocketEvent>,
}

impl RadiusRelayWebSocket for FakeWebSocket {
    fn is_open(&self) -> bool {
        self.state.lock().unwrap().open
    }
    fn buffered_amount(&self) -> usize {
        0
    }
    fn send(&self, frame: RelayFrame) -> ChordResult<()> {
        let mut state = self.state.lock().unwrap();
        if !state.open {
            return Err(ChordError::error("socket is not open"));
        }
        state.sent.push(frame);
        Ok(())
    }
    fn close(&self, code: u16, reason: &str) -> ChordResult<()> {
        if code != 1000 && !(3000..=4999).contains(&code) {
            return Err(ChordError::error("Invalid close code"));
        }
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Ok(());
        }
        (state.closed, state.open) = (true, false);
        let _ = self.events.send(RelaySocketEvent::Close { code, reason: reason.into() });
        Ok(())
    }
}

impl FakeWebSocket {
    fn emit(&self, event: RelaySocketEvent) {
        let _ = self.events.send(event);
    }
    fn open(&self, protocol: &str) {
        self.state.lock().unwrap().open = true;
        self.emit(RelaySocketEvent::Open { protocol: protocol.into() });
    }
    fn message(&self, frame: RelayFrame) {
        self.emit(RelaySocketEvent::Message(frame));
    }
    fn remote_close(&self) {
        let mut state = self.state.lock().unwrap();
        (state.closed, state.open) = (true, false);
        self.emit(RelaySocketEvent::Close { code: 1006, reason: "lost".into() });
    }
    fn fail(&self, message: &str) {
        self.emit(RelaySocketEvent::Error { message: Some(message.into()) });
    }
    fn abnormal_close(&self, message: &str) {
        let mut state = self.state.lock().unwrap();
        (state.closed, state.open) = (true, false);
        self.fail(message);
        self.emit(RelaySocketEvent::Close { code: 1006, reason: String::new() });
    }
    fn sent(&self) -> Vec<RelayFrame> {
        self.state.lock().unwrap().sent.clone()
    }
}

type Sockets = Arc<Mutex<Vec<(Arc<FakeWebSocket>, RadiusRelayWebSocketRequest)>>>;

fn socket_factory() -> (Sockets, RadiusRelayWebSocketFactory) {
    let sockets: Sockets = Arc::default();
    let created = sockets.clone();
    let factory: RadiusRelayWebSocketFactory = Arc::new(move |request| {
        let (events, received) = mpsc::unbounded_channel();
        let socket = Arc::new(FakeWebSocket { state: Mutex::default(), events });
        created.lock().unwrap().push((socket.clone(), request));
        (socket as Arc<dyn RadiusRelayWebSocket>, received)
    });
    (sockets, factory)
}

fn socket(sockets: &Sockets, index: usize) -> (Arc<FakeWebSocket>, RadiusRelayWebSocketRequest) {
    sockets.lock().unwrap()[index].clone()
}

fn token_auth() -> Arc<RadiusRelayAuthResolver> {
    Arc::new(RadiusRelayAuthResolver::new(Some(RadiusAuthInput::Token("secret".into())), None))
}

async fn wait_for(mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

#[test]
fn matches_the_radius_multiplexing_envelope() {
    let payload = [0, 1, 2, 255];
    let parsed = parse_relay_data_frame(&encode_relay_data_frame(CONNECTION_ID, &payload).unwrap()).unwrap();
    assert_eq!(parsed, (CONNECTION_ID.to_string(), payload.to_vec()));
}

#[tokio::test]
async fn bridges_multiplexed_host_clients_into_independent_server_connections() {
    let (sockets, factory) = socket_factory();
    let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let closes = Arc::new(AtomicUsize::new(0));
    let accepted: Arc<Mutex<Option<Arc<dyn ByteConnection>>>> = Arc::default();
    let (record, data, closed) = (accepted.clone(), received.clone(), closes.clone());
    let statuses: Arc<Mutex<Vec<RadiusRelayHostStatus>>> = Arc::default();
    let reported = statuses.clone();
    let host = RadiusRelayHost::new(RadiusRelayHostOptions {
        server_id: SERVER_ID.into(),
        accept: Arc::new(move |connection| {
            *record.lock().unwrap() = Some(connection);
            let (data, closed) = (data.clone(), closed.clone());
            ByteConnectionHandler::new(
                move |chunk| data.lock().unwrap().push(chunk.to_vec()),
                move || {
                    closed.fetch_add(1, Ordering::SeqCst);
                },
                |_| {},
            )
        }),
        auth: token_auth(),
        web_socket_factory: Some(factory),
        on_status: Some(Arc::new(move |status| reported.lock().unwrap().push(status.clone()))),
    });
    host.start();
    wait_for(|| sockets.lock().unwrap().len() == 1).await;
    let (socket, request) = socket(&sockets, 0);
    assert_eq!(request.authorization, "Bearer secret");
    socket.open(&request.protocol);
    wait_for(|| statuses.lock().unwrap().contains(&RadiusRelayHostStatus::Connected)).await;

    let open = json!({ "version": 1, "type": "connection_open", "connection_id": CONNECTION_ID });
    socket.message(RelayFrame::Text(open.to_string()));
    wait_for(|| accepted.lock().unwrap().is_some()).await;
    socket.message(RelayFrame::Binary(encode_relay_data_frame(CONNECTION_ID, &[1, 2, 3]).unwrap()));
    wait_for(|| *received.lock().unwrap() == [vec![1, 2, 3]]).await;

    let connection = accepted.lock().unwrap().clone().unwrap();
    connection.send(vec![4, 5, 6]).await.unwrap();
    let Some(RelayFrame::Binary(outbound)) = socket.sent().last().cloned() else { panic!("expected a data frame") };
    assert_eq!(parse_relay_data_frame(&outbound).unwrap(), (CONNECTION_ID.to_string(), vec![4, 5, 6]));

    let close = json!({ "version": 1, "type": "connection_close", "connection_id": CONNECTION_ID });
    socket.message(RelayFrame::Text(close.to_string()));
    wait_for(|| closes.load(Ordering::SeqCst) == 1).await;
    host.close().await.unwrap();
    assert_eq!(closes.load(Ordering::SeqCst), 1);
}

// Upstream advances fake timers by the 1 s retry delay; here the paused tokio clock auto-advances, and the
// test checks the virtual time that passed.
#[tokio::test(start_paused = true)]
async fn reconnects_the_server_host_after_the_relay_connection_drops() {
    let (sockets, factory) = socket_factory();
    let statuses: Arc<Mutex<Vec<RadiusRelayHostStatus>>> = Arc::default();
    let reported = statuses.clone();
    let host = RadiusRelayHost::new(RadiusRelayHostOptions {
        server_id: SERVER_ID.into(),
        accept: Arc::new(|_| ByteConnectionHandler::new(|_| {}, || {}, |_| {})),
        auth: token_auth(),
        web_socket_factory: Some(factory),
        on_status: Some(Arc::new(move |status| reported.lock().unwrap().push(status.clone()))),
    });
    host.start();
    wait_for(|| sockets.lock().unwrap().len() == 1).await;
    let (socket, request) = socket(&sockets, 0);
    socket.open(&request.protocol);
    wait_for(|| statuses.lock().unwrap().contains(&RadiusRelayHostStatus::Connected)).await;

    let dropped = tokio::time::Instant::now();
    socket.remote_close();
    wait_for(|| sockets.lock().unwrap().len() == 2).await;
    assert!(dropped.elapsed() >= Duration::from_secs(1));
    host.close().await.unwrap();
}

type Calls = (Arc<Mutex<Vec<Vec<u8>>>>, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>);

fn handlers() -> (ByteTransportHandlers, Calls) {
    let calls: Calls = (Arc::default(), Arc::default(), Arc::default());
    let (data, closes, errors) = calls.clone();
    let handlers = ByteTransportHandlers::new(
        move |chunk| data.lock().unwrap().push(chunk.to_vec()),
        move || {
            closes.fetch_add(1, Ordering::SeqCst);
        },
        move |error| errors.lock().unwrap().push(error.to_string()),
    );
    (handlers, calls)
}

#[tokio::test]
async fn uses_a_raw_authenticated_web_socket_as_the_client_byte_transport() {
    let (sockets, factory) = socket_factory();
    let (handlers, (data, closes, errors)) = handlers();
    let transport_factory = create_radius_client_transport_factory(SERVER_ID.into(), token_auth(), Some(factory));
    let opening = tokio::spawn(transport_factory(handlers));
    wait_for(|| sockets.lock().unwrap().len() == 1).await;
    let (socket, request) = socket(&sockets, 0);
    assert_eq!(request.authorization, "Bearer secret");
    socket.open(&request.protocol);
    let transport = opening.await.unwrap().unwrap();

    transport.send(vec![1, 2, 3]).await.unwrap();
    assert_eq!(socket.sent()[0], RelayFrame::Binary(vec![1, 2, 3]));
    socket.message(RelayFrame::Binary(vec![4, 5, 6]));
    wait_for(|| data.lock().unwrap().len() == 1).await;
    assert_eq!(data.lock().unwrap()[0], [4, 5, 6]);
    socket.remote_close();
    wait_for(|| closes.load(Ordering::SeqCst) == 1).await;
    assert!(errors.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reports_established_abnormal_closures_so_the_client_can_reconnect() {
    let (sockets, factory) = socket_factory();
    let (handlers, (_, closes, errors)) = handlers();
    let transport_factory = create_radius_client_transport_factory(SERVER_ID.into(), token_auth(), Some(factory));
    let opening = tokio::spawn(transport_factory(handlers));
    wait_for(|| sockets.lock().unwrap().len() == 1).await;
    let (socket, request) = socket(&sockets, 0);
    socket.open(&request.protocol);
    opening.await.unwrap().unwrap();

    socket.abnormal_close("network lost");
    wait_for(|| errors.lock().unwrap().len() == 1).await;
    assert_eq!(*errors.lock().unwrap(), ["network lost"]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(closes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn reports_a_useful_error_when_the_web_socket_omits_failure_details() {
    let (sockets, factory) = socket_factory();
    let (handlers, _) = handlers();
    let transport_factory = create_radius_client_transport_factory(SERVER_ID.into(), token_auth(), Some(factory));
    let opening = tokio::spawn(transport_factory(handlers));
    wait_for(|| sockets.lock().unwrap().len() == 1).await;
    socket(&sockets, 0).0.fail("");
    let error = opening.await.unwrap().err().expect("opening fails");
    assert_eq!(error.to_string(), "Radius WebSocket connection failed");
}

type StateListener = Arc<dyn Fn(ConnectionState) + Send + Sync>;

#[derive(Default)]
struct FakeClient {
    connected: Mutex<bool>,
    attempts: Mutex<Vec<tokio::time::Instant>>,
    listeners: Mutex<Vec<StateListener>>,
}

impl FakeClient {
    fn set(&self, connected: bool) {
        *self.connected.lock().unwrap() = connected;
        let state = if connected { ConnectionState::Connected } else { ConnectionState::Disconnected };
        let listeners = self.listeners.lock().unwrap().clone();
        for listener in listeners {
            listener(state);
        }
    }
}

#[derive(Clone)]
struct Fake(Arc<FakeClient>);

impl std::ops::Deref for Fake {
    type Target = FakeClient;
    fn deref(&self) -> &FakeClient {
        &self.0
    }
}

impl RadiusReconnectClient for Fake {
    fn attachment_session_id(&self) -> Option<String> {
        Some("demo-1".into())
    }
    fn connected(&self) -> bool {
        *self.connected.lock().unwrap()
    }
    fn connection_state(&self) -> ConnectionState {
        if self.connected() { ConnectionState::Connected } else { ConnectionState::Disconnected }
    }
    fn disconnect(&self, _reason: &str) {
        self.set(false);
    }
    fn reconnect(&self) -> BoxFuture<'static, Result<(), ClientError>> {
        let client = self.clone();
        Box::pin(async move {
            let attempt = {
                let mut attempts = client.attempts.lock().unwrap();
                attempts.push(tokio::time::Instant::now());
                attempts.len()
            };
            if attempt == 1 {
                return Err(ClientError::Other("temporary failure".into()));
            }
            client.set(true);
            Ok(())
        })
    }
    fn on_attachment_change(&self, _listener: Arc<dyn Fn(Option<String>) + Send + Sync>) -> Remove {
        Box::new(|| {})
    }
    fn on_connection_state_change(&self, listener: StateListener) -> Remove {
        self.listeners.lock().unwrap().push(listener);
        Box::new(|| {})
    }
}

// Upstream advances fake timers by 1 s and then 2 s; the paused clock checks the same delays.
#[tokio::test(start_paused = true)]
async fn reconnects_an_established_client_and_restores_its_selected_session() {
    let client = Arc::new(FakeClient { connected: Mutex::new(true), ..FakeClient::default() });
    let reattached: Arc<Mutex<Vec<String>>> = Arc::default();
    let record = reattached.clone();
    let reconnect = RadiusClientReconnect::new(
        Arc::new(Fake(client.clone())),
        Arc::new(move |session_id| {
            record.lock().unwrap().push(session_id);
            Box::pin(async { Ok(()) })
        }),
    );
    let start = tokio::time::Instant::now();
    client.set(false);
    wait_for(|| reattached.lock().unwrap().len() == 1).await;
    let attempts = client.attempts.lock().unwrap().clone();
    assert_eq!(attempts.len(), 2);
    assert!(attempts[0] - start >= Duration::from_secs(1));
    assert!(attempts[1] - attempts[0] >= Duration::from_secs(2));
    assert_eq!(*reattached.lock().unwrap(), ["demo-1"]);
    reconnect.dispose().await;
}
