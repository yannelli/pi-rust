//! Codex WebSocket through a local `CONNECT` proxy in front of a local `ws://` server.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::request::HeaderList;
use super::tests::{codex_model, context, events, mock_token};
use super::*;

#[derive(Clone, Copy)]
enum Mode {
    Tunnel,
    Reject(u16),
    Stall,
}

#[derive(Default)]
struct Seen {
    heads: Vec<String>,
    first_bytes: Vec<u8>,
}

/// A WebSocket server that answers every text frame with a completed Codex response.
async fn ws_target() -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let connections: Arc<Mutex<usize>> = Arc::default();
    let counter = connections.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            *counter.lock().unwrap() += 1;
            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else { return };
                while let Some(Ok(message)) = socket.next().await {
                    if !matches!(message, WsMessage::Text(_)) {
                        continue;
                    }
                    for event in events("response.done", "completed", Some(false)) {
                        let _ = socket.send(WsMessage::Text(event.to_string().into())).await;
                    }
                }
            });
        }
    });
    (address, connections)
}

async fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.unwrap_or(0) == 0 {
            break;
        }
        head.push(byte[0]);
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// A `CONNECT` proxy that records each request head, then tunnels, rejects, or stalls.
async fn proxy(mode: Mode) -> (String, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let shared = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let seen = shared.clone();
            tokio::spawn(async move {
                let mut first = [0u8; 1];
                if client.peek(&mut first).await.unwrap_or(0) == 1 {
                    seen.lock().unwrap().first_bytes.push(first[0]);
                }
                if matches!(mode, Mode::Stall) {
                    return tokio::time::sleep(Duration::from_secs(30)).await;
                }
                let head = read_head(&mut client).await;
                seen.lock().unwrap().heads.push(head.clone());
                let target = head.split_whitespace().nth(1).unwrap_or_default().to_string();
                if let Mode::Reject(code) = mode {
                    let reply =
                        format!("HTTP/1.1 {code} Nope\r\nProxy-Authenticate: Basic\r\nContent-Length: 0\r\n\r\n");
                    return client.write_all(reply.as_bytes()).await.unwrap_or_default();
                }
                let Ok(mut upstream) = TcpStream::connect(target).await else { return };
                let _ = client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await;
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    (address, seen)
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()
}

async fn acquire(target: &str, env: &BTreeMap<String, String>, timeout_ms: u64) -> Result<(), String> {
    let url = format!("ws://{target}/backend-api/codex/responses");
    let headers = HeaderList(Vec::new());
    let lease = websocket::acquire(&url, &headers, None, "acc", env, timeout_ms)
        .await
        .map_err(|error| error.message().to_string())?;
    lease.release(false, None);
    Ok(())
}

#[tokio::test]
async fn streams_over_a_websocket_tunneled_through_the_proxy() {
    let (target, connections) = ws_target().await;
    let (proxy_address, seen) = proxy(Mode::Tunnel).await;
    let model = codex_model(&format!("http://{target}"));
    let options = StreamOptions {
        api_key: Some(mock_token("acc_proxy")),
        transport: Some(Transport::Websocket),
        env: env(&[("HTTP_PROXY", &format!("http://us%40er:p%3Ass@{proxy_address}"))]),
        ..StreamOptions::default()
    };
    let message = OpenAICodexResponses.stream(&model, &context(), options).result(&model).await;
    assert_eq!(
        (message.text(), message.stop_reason),
        ("Hello".to_string(), StopReason::Stop),
        "{:?}",
        message.error_message
    );
    assert!(message.diagnostics.is_empty(), "{:?}", message.diagnostics);
    let head = seen.lock().unwrap().heads[0].clone();
    let credentials = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "us@er:p:ss");
    assert_eq!(
        head,
        format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {credentials}\r\n\r\n")
    );
    assert_eq!(*connections.lock().unwrap(), 1);
}

#[tokio::test]
async fn no_proxy_connects_directly() {
    let (target, connections) = ws_target().await;
    let (proxy_address, seen) = proxy(Mode::Tunnel).await;
    let vars = env(&[("HTTP_PROXY", &format!("http://{proxy_address}")), ("NO_PROXY", "127.0.0.1")]);
    acquire(&target, &vars, 5_000).await.unwrap();
    assert!(seen.lock().unwrap().heads.is_empty());
    assert_eq!(*connections.lock().unwrap(), 1);
    let vars = env(&[("HTTP_PROXY", &format!("http://{proxy_address}")), ("NO_PROXY", "other.test")]);
    acquire(&target, &vars, 5_000).await.unwrap();
    assert_eq!(seen.lock().unwrap().heads.len(), 1);
    assert_eq!(*connections.lock().unwrap(), 2);
}

#[tokio::test]
async fn proxy_rejections_surface_as_unexpected_responses() {
    let (target, connections) = ws_target().await;
    for code in [407, 502] {
        let (proxy_address, seen) = proxy(Mode::Reject(code)).await;
        let vars = env(&[("HTTP_PROXY", &format!("http://{proxy_address}"))]);
        let error = acquire(&target, &vars, 5_000).await.unwrap_err();
        assert_eq!(error, format!("Unexpected server response: {code}"));
        assert!(seen.lock().unwrap().heads[0].starts_with(&format!("CONNECT {target} HTTP/1.1\r\n")));
        assert!(!seen.lock().unwrap().heads[0].contains("Proxy-Authorization"));
    }
    assert_eq!(*connections.lock().unwrap(), 0);
}

#[tokio::test]
async fn the_connect_timeout_covers_the_proxy_and_https_proxies_start_with_tls() {
    let (target, _) = ws_target().await;
    let (proxy_address, _) = proxy(Mode::Stall).await;
    let vars = env(&[("HTTP_PROXY", &format!("http://{proxy_address}"))]);
    let error = acquire(&target, &vars, 100).await.unwrap_err();
    assert_eq!(error, "WebSocket connect timeout after 100ms");

    let (proxy_address, seen) = proxy(Mode::Stall).await;
    let vars = env(&[("HTTP_PROXY", &format!("https://{proxy_address}"))]);
    assert!(acquire(&target, &vars, 200).await.is_err());
    assert_eq!(seen.lock().unwrap().first_bytes, vec![0x16], "a TLS handshake record");

    let socks = env(&[("HTTP_PROXY", "socks5://127.0.0.1:1")]);
    let error = acquire(&target, &socks, 1_000).await.unwrap_err();
    assert!(error.starts_with(crate::http::UNSUPPORTED_PROXY_PROTOCOL_MESSAGE), "{error}");
}
