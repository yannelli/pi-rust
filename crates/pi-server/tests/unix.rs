//! Port of upstream `test/unix.test.ts`, `test/unix-connection.test.ts`, and the Unix cases of
//! `test/server.test.ts`.
#![cfg(unix)]

mod common;

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::SERVER_ID;
use pi_protocol::{
    FrameDecoderOptions, PROTOCOL_VERSION, ProtocolError, ServerHelloError, ServerMessage, ServerMessageDecoder,
    encode_server_message,
};
use pi_server::testing::{TestServerHost, connect_unix_test_client};
use pi_server::transports::unix::{UnixByteConnection, UnixServerOptions, create_unix_server, get_unix_socket_path};
use pi_server::{ByteConnection, Server};
use tokio::io::AsyncReadExt;

const VERSION: u64 = PROTOCOL_VERSION as u64;

fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("ps-").tempdir_in("/tmp").unwrap()
}

fn make_server(path: &Path) -> Server<TestServerHost> {
    create_unix_server(TestServerHost::new(), UnixServerOptions::new(SERVER_ID, path)).unwrap()
}

fn identity(path: &Path) -> (u64, u64) {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    assert!(metadata.file_type().is_socket());
    (metadata.dev(), metadata.ino())
}

async fn assert_hello(path: &Path) {
    let client = connect_unix_test_client(path).await.unwrap();
    let reply = client.hello(VERSION).await.unwrap();
    assert!(matches!(reply, ServerMessage::Hello(ref hello) if hello.server_id == SERVER_ID));
    client.close().await;
}

#[tokio::test]
async fn derives_the_socket_path_and_supports_replacement_servers() {
    let directory = temp_dir();
    let path = get_unix_socket_path(SERVER_ID, directory.path()).unwrap();
    assert_eq!(path, directory.path().join(format!("{SERVER_ID}.sock")));
    assert!(get_unix_socket_path("server-1", directory.path()).is_err());

    let first = make_server(&path);
    first.start().await.unwrap();
    assert_hello(&path).await;
    first.close().await.unwrap();

    let replacement = make_server(&path);
    replacement.start().await.unwrap();
    assert_hello(&path).await;
    replacement.close().await.unwrap();
}

#[tokio::test]
async fn rejects_concurrent_start_calls_without_leaking_the_listener() {
    let directory = temp_dir();
    let path = directory.path().join("server.sock");
    let server = make_server(&path);
    let (first, second) = tokio::join!(server.start(), server.start());
    first.unwrap();
    assert!(second.unwrap_err().to_string().contains("starting"));
    server.close().await.unwrap();
    assert!(std::fs::symlink_metadata(&path).is_err());
}

#[test]
fn rejects_invalid_unix_server_options() {
    let path = PathBuf::from("/tmp/pi-server-timeout-test.sock");
    let mut options = UnixServerOptions::new(SERVER_ID, &path);
    options.handshake_timeout = Some(Duration::from_millis(2_147_483_648));
    let error = create_unix_server(TestServerHost::new(), options).err().unwrap();
    assert!(error.to_string().contains("handshakeTimeoutMs"));

    let mut options = UnixServerOptions::new(SERVER_ID, &path);
    options.graceful_close_timeout = Some(Duration::from_millis(2_147_483_648));
    let error = create_unix_server(TestServerHost::new(), options).err().unwrap();
    assert!(error.to_string().contains("gracefulCloseTimeoutMs"));

    let mut options = UnixServerOptions::new(SERVER_ID, &path);
    options.max_frame_length = Some(128);
    options.max_pending_bytes = Some(131);
    let error = create_unix_server(TestServerHost::new(), options).err().unwrap();
    assert!(error.to_string().contains("maxPendingBytes"));
}

#[tokio::test]
async fn rejects_a_live_listener_without_unlinking_it() {
    let directory = temp_dir();
    let path = directory.path().join("server.sock");
    let first = make_server(&path);
    first.start().await.unwrap();
    let original = identity(&path);

    let second = make_server(&path);
    assert!(second.start().await.unwrap_err().to_string().contains("already running"));
    assert_eq!(identity(&path), original);
    assert_hello(&path).await;
    first.close().await.unwrap();
}

#[tokio::test]
async fn never_unlinks_a_regular_file_at_the_configured_path() {
    let directory = temp_dir();
    let path = directory.path().join("server.sock");
    std::fs::write(&path, "do not remove").unwrap();
    let server = make_server(&path);
    assert!(server.start().await.unwrap_err().to_string().contains("non-socket"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "do not remove");
}

#[tokio::test]
async fn creates_nested_parents_restricts_permissions_and_removes_its_socket() {
    let directory = temp_dir();
    let path = directory.path().join("p").join("n").join("server.sock");
    let server = make_server(&path);
    server.start().await.unwrap();
    identity(&path);
    assert_eq!(std::fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let entries: Vec<_> =
        std::fs::read_dir(path.parent().unwrap()).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("server.sock")]);
    server.close().await.unwrap();
    assert!(std::fs::symlink_metadata(&path).is_err());
}

#[tokio::test]
async fn does_not_remove_a_replacement_inode_during_shutdown() {
    let directory = temp_dir();
    let path = directory.path().join("server.sock");
    let server = make_server(&path);
    server.start().await.unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "replacement").unwrap();
    server.close().await.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
}

#[tokio::test]
async fn removes_a_genuinely_stale_socket_before_binding() {
    let directory = temp_dir();
    let path = directory.path().join("server.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    identity(&path);

    let server = make_server(&path);
    server.start().await.unwrap();
    identity(&path);
    assert_hello(&path).await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn queues_a_final_protocol_error_behind_pending_output_before_closing() {
    let (local, mut peer) = tokio::net::UnixStream::pair().unwrap();
    let (_reader, writer) = local.into_split();
    let connection = UnixByteConnection::new(writer, Duration::from_secs(1), 64 * 1024);
    let pending = connection.send(vec![1, 2, 3]);
    let final_message = ServerMessage::HelloError(ServerHelloError {
        error: ProtocolError::new("invalid_request", "Protocol violation"),
    });
    let final_frame = encode_server_message(&final_message, &FrameDecoderOptions::default()).unwrap();
    let closing = connection.close(Some(final_frame));
    assert!(connection.send(vec![4]).await.is_err());
    pending.await.unwrap();

    let mut received = Vec::new();
    peer.read_to_end(&mut received).await.unwrap();
    assert_eq!(&received[..3], &[1, 2, 3]);
    assert_eq!(ServerMessageDecoder::default().push(&received[3..]).unwrap(), vec![final_message]);
    connection.mark_closed();
    closing.await.unwrap();
    assert!(connection.closed());
}
