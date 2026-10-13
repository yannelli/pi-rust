//! Port of upstream `test/unix-transport.test.ts` and `test/unix.test.ts`.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::json;
use pi_chord::services::{ServiceCall, parse_service_call};
use pi_client::unix::{
    DiscoverUnixServersOptions, UnixServerRoute, UnixTransportOptions, create_unix_transport_factory,
    discover_unix_servers,
};
use pi_client::{ByteTransportHandlers, Client, ClientOptions, ConnectionState};
use pi_protocol::{
    ClientMessage, ClientMessageDecoder, FrameDecoderOptions, ResponseEnvelope, RpcTarget, ServerHello, ServerMessage,
    encode_server_message,
};
use pi_server::testing::TestServerHost;
use pi_server::transports::unix::{UnixListenerOptions, create_unix_listener};
use pi_server::{Server, ServerOptions};
use support::SERVER_ID;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("pc-").tempdir_in("/tmp").unwrap()
}

fn frame(message: ServerMessage) -> Vec<u8> {
    encode_server_message(&message, &FrameDecoderOptions::default()).unwrap()
}

fn list_call() -> ServiceCall {
    ServiceCall { service_id: "test.server".into(), instance: None, member: "list".into(), args: vec![] }
}

/// Serve raw sockets, handing each decoded client message to `respond`.
fn serve(path: &Path, respond: impl Fn(ClientMessage) -> Vec<Vec<u8>> + Send + Sync + 'static) {
    let listener = UnixListener::bind(path).unwrap();
    let respond = Arc::new(respond);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let respond = respond.clone();
            tokio::spawn(async move {
                let mut decoder = ClientMessageDecoder::default();
                let mut buffer = vec![0u8; 4096];
                while let Ok(count) = socket.read(&mut buffer).await {
                    if count == 0 {
                        return;
                    }
                    for message in decoder.push(&buffer[..count]).unwrap() {
                        for chunk in respond(message) {
                            if chunk.is_empty() {
                                let _ = socket.shutdown().await;
                                return;
                            }
                            socket.write_all(&chunk).await.unwrap();
                        }
                    }
                }
            });
        }
    });
}

#[test]
fn rejects_invalid_unix_transport_options() {
    let empty = create_unix_transport_factory(UnixTransportOptions::new("")).err().unwrap();
    assert!(empty.to_string().contains("must not be empty"));
    let options = UnixTransportOptions { path: "/tmp/pi.sock".into(), max_pending_bytes: Some(0) };
    assert!(create_unix_transport_factory(options).err().unwrap().to_string().contains("positive"));
}

#[tokio::test]
async fn carries_a_handshake_and_request_over_a_real_unix_socket() {
    let directory = temp_dir();
    let path = directory.path().join("pi.sock");
    let received = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = received.clone();
    serve(&path, move |message| match message {
        ClientMessage::Hello(_) => frame(ServerMessage::Hello(ServerHello { server_id: SERVER_ID.into() }))
            .into_iter()
            .map(|b| vec![b])
            .collect(),
        ClientMessage::Request(request) => {
            let call = parse_service_call(&request.call).unwrap();
            recorded.lock().unwrap().push(format!("{}.{}", call.service_id, call.member));
            let reply =
                frame(ServerMessage::Response(ResponseEnvelope { id: request.id, result: Ok(Some(json!([]))) }));
            let split = reply.len() / 2;
            vec![reply[..split].to_vec(), reply[split..].to_vec()]
        }
        ClientMessage::Cancel(_) => vec![],
    });
    let factory = create_unix_transport_factory(UnixTransportOptions::new(&path)).unwrap();
    let client = Client::new(ClientOptions::new(SERVER_ID, factory)).unwrap();
    assert_eq!(client.connect().await.unwrap().server_id, SERVER_ID);
    let result = client.request(RpcTarget::server(SERVER_ID), list_call(), None).await.unwrap();
    assert_eq!(result, Some(json!([])));
    assert_eq!(*received.lock().unwrap(), vec!["test.server.list".to_string()]);
    client.dispose();
}

#[tokio::test]
async fn reports_truncated_final_frames_through_the_client() {
    let directory = temp_dir();
    let path = directory.path().join("pi.sock");
    serve(&path, |message| match message {
        ClientMessage::Hello(_) => vec![frame(ServerMessage::Hello(ServerHello { server_id: SERVER_ID.into() }))],
        _ => vec![vec![0, 0, 0, 2, 1], vec![]],
    });
    let factory = create_unix_transport_factory(UnixTransportOptions::new(&path)).unwrap();
    let client = Client::new(ClientOptions::new(SERVER_ID, factory)).unwrap();
    client.connect().await.unwrap();
    let error = client.request(RpcTarget::server(SERVER_ID), list_call(), None).await.unwrap_err();
    assert_eq!(error.name(), "ProtocolValidationError");
    assert!(error.to_string().to_lowercase().contains("truncated"));
    assert_eq!(client.connection_state(), ConnectionState::Disconnected);
    client.dispose();
}

#[tokio::test]
async fn rejects_connection_attempts_to_missing_sockets() {
    let directory = temp_dir();
    let factory = create_unix_transport_factory(UnixTransportOptions::new(directory.path().join("pi.sock"))).unwrap();
    let handlers = ByteTransportHandlers::new(|_| {}, || {}, |_| {});
    let error = factory(handlers).await.err().unwrap();
    assert_eq!(error.io_kind(), Some(std::io::ErrorKind::NotFound));
}

fn server_id(value: u32) -> String {
    format!("00000000-0000-4000-8000-{value:012x}")
}

async fn start_server(directory: &Path, file_id: &str, reported_id: &str) -> Server<TestServerHost> {
    let listener = create_unix_listener(UnixListenerOptions::new(directory.join(format!("{file_id}.sock")))).unwrap();
    let server = Server::new(TestServerHost::new(), ServerOptions::new(reported_id, vec![listener])).unwrap();
    server.start().await.unwrap();
    server
}

/// Accept and hold connections without answering, counting them.
fn silent_socket(path: PathBuf, active: Arc<AtomicUsize>, maximum: Arc<AtomicUsize>, total: Arc<AtomicUsize>) {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(now, Ordering::SeqCst);
            total.fetch_add(1, Ordering::SeqCst);
            let active = active.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 256];
                while matches!(socket.read(&mut buffer).await, Ok(count) if count > 0) {}
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
}

fn discover(directory: &Path, timeout: Option<Duration>) -> DiscoverUnixServersOptions {
    DiscoverUnixServersOptions { directory: directory.to_path_buf(), timeout }
}

#[tokio::test]
async fn returns_no_routes_when_the_directory_is_missing() {
    let directory = temp_dir();
    assert!(discover_unix_servers(discover(&directory.path().join("missing"), None)).await.unwrap().is_empty());
}

#[tokio::test]
async fn discovers_reachable_servers_in_server_id_order() {
    let directory = temp_dir();
    let (first, second) = (server_id(1), server_id(2));
    let servers =
        [start_server(directory.path(), &second, &second).await, start_server(directory.path(), &first, &first).await];
    let routes = discover_unix_servers(discover(directory.path(), None)).await.unwrap();
    let route = |id: &str| UnixServerRoute { server_id: id.into(), path: directory.path().join(format!("{id}.sock")) };
    assert_eq!(routes, vec![route(&first), route(&second)]);
    for server in servers {
        server.close().await.unwrap();
    }
}

#[tokio::test]
async fn ignores_malformed_entries_non_sockets_and_mismatched_servers() {
    let directory = temp_dir();
    std::fs::write(directory.path().join(format!("{}.sock", server_id(1))), "not a socket").unwrap();
    std::fs::write(directory.path().join("not-a-server.sock"), "ignored").unwrap();
    std::fs::create_dir(directory.path().join(format!("{}.sock", server_id(2)))).unwrap();
    let server = start_server(directory.path(), &server_id(3), &server_id(4)).await;
    assert!(discover_unix_servers(discover(directory.path(), None)).await.unwrap().is_empty());
    server.close().await.unwrap();
}

#[tokio::test]
async fn ignores_stale_sockets_without_deleting_them() {
    let directory = temp_dir();
    let path = directory.path().join(format!("{}.sock", server_id(1)));
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(discover_unix_servers(discover(directory.path(), None)).await.unwrap().is_empty());
    assert!(std::os::unix::fs::FileTypeExt::is_socket(&std::fs::symlink_metadata(&path).unwrap().file_type()));
}

#[tokio::test]
async fn times_out_an_unresponsive_socket_without_deleting_it() {
    let directory = temp_dir();
    let path = directory.path().join(format!("{}.sock", server_id(1)));
    let counters = (Arc::default(), Arc::default(), Arc::default());
    silent_socket(path.clone(), counters.0, counters.1, counters.2);
    let routes = discover_unix_servers(discover(directory.path(), Some(Duration::from_millis(20)))).await.unwrap();
    assert!(routes.is_empty());
    assert!(path.exists());
}

#[tokio::test]
async fn limits_concurrent_probes_to_16() {
    let directory = temp_dir();
    let (active, maximum, total) =
        (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    for index in 1..=20 {
        let path = directory.path().join(format!("{}.sock", server_id(index)));
        silent_socket(path, active.clone(), maximum.clone(), total.clone());
    }
    let options = discover(directory.path(), Some(Duration::from_millis(100)));
    let discovery = tokio::spawn(discover_unix_servers(options));
    support::poll(|| active.load(Ordering::SeqCst) == 16).await;
    assert!(discovery.await.unwrap().unwrap().is_empty());
    assert_eq!(maximum.load(Ordering::SeqCst), 16);
    assert_eq!(total.load(Ordering::SeqCst), 20);
}

#[tokio::test]
async fn ignores_an_endpoint_that_closes_before_its_handshake() {
    let directory = temp_dir();
    let listener = UnixListener::bind(directory.path().join(format!("{}.sock", server_id(1)))).unwrap();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
        }
    });
    assert!(discover_unix_servers(discover(directory.path(), None)).await.unwrap().is_empty());
}

#[tokio::test]
async fn propagates_unexpected_filesystem_errors() {
    let directory = temp_dir();
    let file = directory.path().join("not-a-directory");
    std::fs::write(&file, "content").unwrap();
    let error = discover_unix_servers(discover(&file, None)).await.unwrap_err();
    assert_eq!(error.io_kind(), Some(std::io::ErrorKind::NotADirectory));
}
