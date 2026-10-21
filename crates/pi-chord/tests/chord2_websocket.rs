//! Port of upstream `test/websocket.test.ts`: a browser host over a real socket stamped by the authenticator, and
//! packed payloads as binary messages. The client's own socket is read through a tee of its TCP stream.

mod chord2_support;

use std::io::{Read, Write};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use chord2_support::net::*;
use chord2_support::*;
use futures::{SinkExt, StreamExt};
use pi_chord::context::{AbortController, AbortSignal};
use pi_chord::host2::link::inflate_message;
use pi_chord::host2::wire::PeerKind;
use pi_chord::host2::{
    Authenticator, ConnectOptions, Contract, Declare, Host, HostOptions, Implementation, Link, RemoteDef, RootOptions,
    ServeOptions, VariantDef, WebSocketAcceptOptions, WebSocketLinkOptions, accept_web_sockets, connect_web_socket,
    create_host, host_named, method, on_value, remote, web_socket_link,
};
use pi_chord::{ChordError, ChordResult, Json, json};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::Request;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocket};

fn board() -> Contract {
    let main = VariantDef::new()
        .state("notes", array(string()))
        .method("add", method(string(), None))
        .method("who", method(None, unknown()));
    let def = RemoteDef {
        version: 1,
        input: object(&[]),
        host: host_named("server").unwrap(),
        variants: vec![("main".into(), main)],
    };
    remote("t.board", &def).unwrap()
}

fn server() -> Host {
    create_host(host_options("server", &Log::default()))
}

/// Accepts WebSockets on a fresh loopback port: the task and the port's URL.
async fn listen(
    server: &Host,
    authenticate: impl Fn(&Request) -> Option<Json> + Send + Sync + 'static,
) -> (JoinHandle<()>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/", listener.local_addr().unwrap());
    let authenticator: Authenticator = Arc::new(move |request| Box::pin(std::future::ready(Ok(authenticate(request)))));
    (accept_web_sockets(listener, server.clone(), WebSocketAcceptOptions::new(authenticator)), url)
}

/// A host of `kind` whose parent is reached over `link`.
fn host_on(name: &str, kind: PeerKind, link: Arc<dyn Link>) -> Host {
    let upstream = Some((link, ConnectOptions::new(json!({"kind": "server"}))));
    create_host(HostOptions { kind, upstream, ..host_options(name, &Log::default()) })
}

/// Upstream `AbortSignal.timeout(ms)`.
fn timeout_signal(ms: u64) -> AbortSignal {
    let controller = AbortController::new();
    let signal = controller.signal();
    tokio::spawn(async move {
        sleep_ms(ms).await;
        controller.abort(None);
    });
    signal
}

/// Calls `add("x")` on a board held by `host`, in a root that gives up after `ms`.
async fn add_with_timeout(host: &Host, ms: u64) -> ChordResult<Option<Json>> {
    let board = host.hold(&board(), json!({}), Default::default()).unwrap();
    let run = move |ctx| async move { board.call("add", Some(json!("x")), Some(&ctx)).await };
    host.root(run, RootOptions { signal: Some(timeout_signal(ms)), ..RootOptions::default() }).await
}

fn assert_aborted(result: ChordResult<Option<Json>>) {
    let error: ChordError = result.unwrap_err();
    assert!(is(&error, "aborted"), "{error}");
}

#[tokio::test]
async fn serves_a_browser_style_client_over_a_real_socket_stamped_by_the_authenticator() {
    let _serial = serial().await;
    let server = server();
    let factory = |_: &Json, principal: &Json, decl: &mut Declare| {
        let notes = decl.state("notes", array(string()), json!([]))?;
        let principal = principal.clone();
        Ok(Implementation::variant("main")
            .state("notes", notes.clone())
            .method("add", move |text, ctx| {
                let notes = notes.clone();
                async move {
                    notes.change(&ctx, |draft| draft.push(text))?;
                    Ok(Json::Null)
                }
            })
            .method("who", move |_, _| std::future::ready(Ok(principal.clone()))))
    };
    server.serve(&board(), factory, ServeOptions::default()).unwrap();
    let (accepting, url) = listen(&server, |request| {
        let query = request.uri().query().unwrap_or_default();
        let token = query.split('&').find_map(|pair| pair.strip_prefix("token="));
        (token == Some("ok")).then(|| json!({"guest": "web"}))
    })
    .await;
    let open = |token: &str| {
        let link = connect_web_socket(&format!("{url}?token={token}"), WebSocketLinkOptions::default());
        host_on("browser", PeerKind::Client, link)
    };

    let browser = open("ok");
    let board = browser.hold(&board(), json!({}), Default::default()).unwrap();
    let seen: Log<Json> = Log::default();
    let notes = board.state("notes").unwrap();
    let pushed = seen.clone();
    let _seen = on_value(notes.as_ref(), move |value| pushed.push(value.clone()));
    call(&board, "add", json!("hello")).await.unwrap();
    assert_eq!(call(&board, "who", Json::Null).await.unwrap(), json!({"guest": "web"}));
    for _ in 0..50 {
        if notes.value().ok().and_then(|value| value.as_array().map(|items| items.len())) == Some(1) {
            break;
        }
        sleep_ms(5).await;
    }
    assert_eq!(notes.value().unwrap(), json!(["hello"]));
    assert_eq!(seen.all().last(), Some(&json!(["hello"])));
    browser.close().await.unwrap();

    // WebSocket peers are always clients: one announcing itself as a host is cut off.
    let link = connect_web_socket(&format!("{url}?token=ok"), WebSocketLinkOptions::default());
    let impostor = host_on("session:S1", PeerKind::Host, link);
    assert_aborted(add_with_timeout(&impostor, 150).await);
    impostor.close().await.unwrap();

    // The server closes unauthenticated sockets; calls wait for a reconnect until their ctx gives up.
    let refused = open("bad");
    assert_aborted(add_with_timeout(&refused, 100).await);
    refused.close().await.unwrap();
    accepting.abort();
}

/// A TCP stream that keeps a copy of everything it reads.
struct Tee {
    inner: TcpStream,
    seen: Arc<Mutex<Vec<u8>>>,
}

impl AsyncRead for Tee {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            this.seen.lock().unwrap().extend_from_slice(&buf.filled()[before..]);
        }
        poll
    }
}

impl AsyncWrite for Tee {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Recorded server bytes replayed to a WebSocket parser; what it writes back is dropped.
struct Replay(std::io::Cursor<Vec<u8>>);

impl Read for Replay {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Replay {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The binary messages among the bytes a client socket read (its upgrade response first).
fn binary_messages(bytes: &[u8]) -> Vec<Vec<u8>> {
    let start = bytes.windows(4).position(|window| window == b"\r\n\r\n").expect("an upgrade response") + 4;
    let replay = Replay(std::io::Cursor::new(bytes[start..].to_vec()));
    let mut socket = WebSocket::from_raw_socket(replay, Role::Client, None);
    let mut binary = Vec::new();
    while let Ok(message) = socket.read() {
        if let Message::Binary(data) = message {
            binary.push(data.to_vec());
        }
    }
    binary
}

#[tokio::test]
async fn large_snapshots_and_results_reach_the_client_as_binary_messages_and_clients_may_not_send_them() {
    let _serial = serial().await;
    let notes: Vec<Json> = (0..40).map(|i| json!(format!("{i}:{}", "x".repeat(4000)))).collect();
    let notes = Json::array(notes);
    let server = server();
    let initial = notes.clone();
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let state = decl.state("notes", array(string()), initial.clone())?;
        let result = initial.clone();
        Ok(Implementation::variant("main")
            .state("notes", state)
            .method("add", |_, _| std::future::ready(Ok(Json::Null)))
            .method("who", move |_, _| std::future::ready(Ok(result.clone()))))
    };
    server.serve(&board(), factory, ServeOptions::default()).unwrap();
    let (accepting, url) = listen(&server, |_| Some(json!({"guest": "web"}))).await;

    let seen: Arc<Mutex<Vec<u8>>> = Arc::default();
    let stream = TcpStream::connect(url.trim_start_matches("ws://").trim_end_matches('/')).await.unwrap();
    let tee = Tee { inner: stream, seen: seen.clone() };
    let (socket, _) = tokio_tungstenite::client_async(url.as_str(), tee).await.unwrap();
    let browser = host_on("browser", PeerKind::Client, web_socket_link(socket, WebSocketLinkOptions::default()));
    let board = browser.hold(&board(), json!({}), Default::default()).unwrap();
    assert_eq!(call(&board, "who", Json::Null).await.unwrap(), notes);
    let state = board.state("notes").unwrap();
    let _subscribed = on_value(state.as_ref(), |_| {});
    for _ in 0..100 {
        if state.hydrated() {
            break;
        }
        sleep_ms(5).await;
    }
    assert_eq!(state.value().unwrap(), notes);
    let binary = binary_messages(&seen.lock().unwrap());
    assert!(binary.len() >= 2, "the result and the snapshot came as binary messages");
    // A text-only transport (the TUI launcher) turns a packed message back into its text frame.
    let frames: Vec<Json> =
        binary.iter().map(|message| Json::parse(&inflate_message(message).unwrap()).unwrap()).collect();
    assert!(frames.iter().any(|frame| frame["t"] == json!("result") && frame["value"] == notes));
    assert!(frames.iter().any(|frame| frame["t"] == json!("state") && frame["value"] == notes));
    browser.close().await.unwrap();

    // A client's binary message is a protocol error: the server closes the socket.
    let (mut raw, _) = tokio_tungstenite::connect_async(url.as_str()).await.unwrap();
    raw.send(Message::binary(vec![0, 0, 0, 2, 123, 125])).await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(message)) = raw.next().await {
            if let Message::Close(_) = message {
                break;
            }
        }
    });
    closed.await.expect("the server closes the socket");
    accepting.abort();
}
