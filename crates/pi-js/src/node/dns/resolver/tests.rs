use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio_util::sync::CancellationToken;

use super::super::wire::Kind;
use super::Resolver;

const ANY: (Ipv4Addr, Ipv6Addr) = (Ipv4Addr::UNSPECIFIED, Ipv6Addr::UNSPECIFIED);

/// Builds a reply to `query`: same ID and question, one A or CNAME answer when given.
fn reply(query: &[u8], rcode: u8, truncated: bool, answer: Option<Answer>) -> Vec<u8> {
    let mut end = 12;
    while query[end] != 0 {
        end += usize::from(query[end]) + 1;
    }
    end += 5;
    let mut out = query[..2].to_vec();
    out.push(0x81 | if truncated { 0x02 } else { 0 });
    out.push(0x80 | rcode);
    out.extend_from_slice(&[0, 1, 0, u8::from(answer.is_some()), 0, 0, 0, 0]);
    out.extend_from_slice(&query[12..end]);
    match answer {
        Some(Answer::A(address)) => {
            out.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
            out.extend_from_slice(&address);
        }
        Some(Answer::Cname(target, ttl)) => {
            let mut data = Vec::new();
            for label in target.split('.') {
                data.push(label.len() as u8);
                data.extend_from_slice(label.as_bytes());
            }
            data.push(0);
            out.extend_from_slice(&[0xC0, 12, 0, 5, 0, 1]);
            out.extend_from_slice(&ttl.to_be_bytes());
            out.extend_from_slice(&[0, data.len() as u8]);
            out.extend_from_slice(&data);
        }
        None => {}
    }
    out
}

#[derive(Clone)]
enum Answer {
    A([u8; 4]),
    Cname(&'static str, u32),
}

fn first_label(query: &[u8]) -> String {
    String::from_utf8_lossy(&query[13..13 + usize::from(query[12])]).into_owned()
}

type Handler = Arc<dyn Fn(&[u8]) -> Vec<Vec<u8>> + Send + Sync>;

/// UDP server on 127.0.0.1 that sends every datagram `handler` returns; counts queries.
async fn udp_server(handler: Handler) -> (SocketAddr, Arc<AtomicUsize>) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        let mut buffer = vec![0u8; 4096];
        while let Ok((size, peer)) = socket.recv_from(&mut buffer).await {
            counter.fetch_add(1, Ordering::SeqCst);
            for packet in handler(&buffer[..size]) {
                let _ = socket.send_to(&packet, peer).await;
            }
        }
    });
    (address, hits)
}

fn resolver(servers: Vec<SocketAddr>, millis: u64, tries: usize) -> Resolver {
    Resolver::new(servers, Duration::from_millis(millis), tries, None)
}

async fn ask(servers: Vec<SocketAddr>, millis: u64, tries: usize, name: &str) -> Result<Value, &'static str> {
    resolver(servers, millis, tries).resolve(name, Kind::A, false, ANY, CancellationToken::new()).await
}

#[tokio::test]
async fn ignores_mismatched_packets_before_the_answer() {
    let handler: Handler = Arc::new(|query| {
        let mut wrong = reply(query, 0, false, Some(Answer::A([9, 9, 9, 9])));
        wrong[0] ^= 0xFF;
        vec![wrong, reply(query, 0, false, Some(Answer::A([1, 2, 3, 4])))]
    });
    let (server, _) = udp_server(handler).await;
    let found = ask(vec![server], 1000, 1, "a.test");
    assert_eq!(found.await, Ok(json!(["1.2.3.4"])));
}

#[tokio::test]
async fn mismatched_packets_do_not_extend_the_deadline() {
    let handler: Handler = Arc::new(|query| {
        let mut wrong = reply(query, 0, false, None);
        wrong[1] ^= 0xFF;
        vec![wrong; 50]
    });
    let (server, _) = udp_server(handler).await;
    let started = std::time::Instant::now();
    let found = ask(vec![server], 150, 1, "a.test");
    assert_eq!(found.await, Err("ETIMEOUT"));
    assert!(started.elapsed() < Duration::from_millis(600));
}

#[tokio::test]
async fn truncated_reply_falls_back_to_tcp_on_the_same_server() {
    let (server, hits) = udp_server(Arc::new(|query| vec![reply(query, 0, true, Some(Answer::A([9, 9, 9, 9])))])).await;
    let listener = TcpListener::bind(server).await.unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut prefix = [0u8; 2];
        stream.read_exact(&mut prefix).await.unwrap();
        let mut query = vec![0u8; usize::from(u16::from_be_bytes(prefix))];
        stream.read_exact(&mut query).await.unwrap();
        let body = reply(&query, 0, false, Some(Answer::A([5, 6, 7, 8])));
        let mut framed = (body.len() as u16).to_be_bytes().to_vec();
        framed.extend_from_slice(&body);
        stream.write_all(&framed).await.unwrap();
    });
    let found = ask(vec![server], 1000, 1, "a.test");
    assert_eq!(found.await, Ok(json!(["5.6.7.8"])));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn every_rcode_ends_the_query_on_the_first_server() {
    for (rcode, code) in [(1, "EFORMERR"), (2, "ESERVFAIL"), (3, "ENOTFOUND"), (4, "ENOTIMP"), (5, "EREFUSED")] {
        let (first, _) = udp_server(Arc::new(move |query| vec![reply(query, rcode, false, None)])).await;
        let (second, asked) = udp_server(Arc::new(|query| vec![reply(query, 0, false, None)])).await;
        assert_eq!(ask(vec![first, second], 1000, 2, "a.test").await, Err(code));
        assert_eq!(asked.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn malformed_reply_moves_to_the_next_server_at_once() {
    let (broken, _) = udp_server(Arc::new(|query| {
        let packet = reply(query, 0, false, Some(Answer::A([9, 9, 9, 9])));
        vec![packet[..packet.len() - 3].to_vec()]
    }))
    .await;
    let (good, _) = udp_server(Arc::new(|query| vec![reply(query, 0, false, Some(Answer::A([1, 1, 1, 1])))])).await;
    let started = std::time::Instant::now();
    assert_eq!(ask(vec![broken, good], 5000, 1, "a.test").await, Ok(json!(["1.1.1.1"])));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(ask(vec![broken], 5000, 1, "a.test").await, Err("EBADRESP"));
}

#[tokio::test]
async fn timeouts_rotate_servers_and_respect_max_timeout() {
    let (first, one) = udp_server(Arc::new(|_| Vec::new())).await;
    let (second, two) = udp_server(Arc::new(|_| Vec::new())).await;
    let capped = Resolver::new(vec![first, second], Duration::from_secs(5), 2, Some(Duration::from_millis(60)));
    let started = std::time::Instant::now();
    let found = capped.resolve("a.test", Kind::A, false, ANY, CancellationToken::new()).await;
    assert_eq!(found, Err("ETIMEOUT"));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!((one.load(Ordering::SeqCst), two.load(Ordering::SeqCst)), (2, 2));
}

#[test]
fn attempt_wait_follows_c_ares() {
    let millis = |resolver: &Resolver, round| resolver.attempt_wait(round).unwrap().as_millis();
    assert_eq!(millis(&resolver(Vec::new(), 100, 1), 0), 250);
    assert_eq!(millis(&resolver(Vec::new(), 10_000, 1), 0), 5000);
    let doubled = millis(&resolver(Vec::new(), 300, 1), 1);
    assert!((300..=600).contains(&doubled), "{doubled}");
    let capped = Resolver::new(Vec::new(), Duration::from_millis(100), 1, Some(Duration::from_millis(150)));
    assert_eq!((millis(&capped, 0), millis(&capped, 3)), (150, 150));
    let swapped = capped.with_servers(vec![SocketAddr::from(([127, 0, 0, 1], 53))]);
    assert_eq!((swapped.servers.len(), swapped.max_timeout, swapped.tries), (1, capped.max_timeout, 1));
}

#[tokio::test]
async fn follows_aliases_and_stops_on_a_loop() {
    let handler: Handler = Arc::new(|query| {
        let answer = match first_label(query).as_str() {
            "start" => Answer::Cname("end.test", 60),
            "end" => Answer::A([4, 4, 4, 4]),
            "loop" => Answer::Cname("back.test", 60),
            _ => Answer::Cname("loop.test", 60),
        };
        vec![reply(query, 0, false, Some(answer))]
    });
    let (server, _) = udp_server(handler).await;
    let resolver = resolver(vec![server], 1000, 1);
    let found = resolver.resolve("start.test", Kind::A, false, ANY, CancellationToken::new()).await;
    assert_eq!(found, Ok(json!(["4.4.4.4"])));
    let looped = resolver.resolve("loop.test", Kind::A, false, ANY, CancellationToken::new()).await;
    assert_eq!(looped, Err("ENODATA"));
}

#[tokio::test]
async fn address_ttl_is_clamped_to_the_shortest_alias_ttl() {
    let handler: Handler = Arc::new(|query| {
        let answer = match first_label(query).as_str() {
            "start" => Answer::Cname("mid.test", 100),
            "mid" => Answer::Cname("end.test", 20),
            _ => Answer::A([4, 4, 4, 4]),
        };
        vec![reply(query, 0, false, Some(answer))]
    });
    let (server, _) = udp_server(handler).await;
    let resolver = resolver(vec![server], 1000, 1);
    let found = resolver.resolve("start.test", Kind::A, true, ANY, CancellationToken::new()).await;
    assert_eq!(found, Ok(json!([{ "address": "4.4.4.4", "ttl": 20 }])));
    let direct = resolver.resolve("end.test", Kind::A, true, ANY, CancellationToken::new()).await;
    assert_eq!(direct, Ok(json!([{ "address": "4.4.4.4", "ttl": 60 }])));
}

#[tokio::test]
async fn cancel_ends_a_pending_wait() {
    let (server, hits) = udp_server(Arc::new(|_| Vec::new())).await;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let started = std::time::Instant::now();
    let found = resolver(vec![server], 10_000, 4).resolve("a.test", Kind::A, false, ANY, cancel).await;
    assert_eq!(found, Err("ECANCELLED"));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_calls_get_their_own_answers() {
    let handler: Handler = Arc::new(|query| {
        let last = if first_label(query) == "one" { 1 } else { 2 };
        vec![reply(query, 0, false, Some(Answer::A([10, 0, 0, last])))]
    });
    let (server, _) = udp_server(handler).await;
    let resolver = resolver(vec![server], 1000, 1);
    let (one, two) = tokio::join!(
        resolver.resolve("one.test", Kind::A, false, ANY, CancellationToken::new()),
        resolver.resolve("two.test", Kind::A, false, ANY, CancellationToken::new()),
    );
    assert_eq!((one, two), (Ok(json!(["10.0.0.1"])), Ok(json!(["10.0.0.2"]))));
}

#[tokio::test]
async fn no_servers_and_refused_ports() {
    assert_eq!(ask(Vec::new(), 100, 1, "a.test").await, Err("UNKNOWN_ARES_ERROR"));
    let closed = UdpSocket::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let (good, _) = udp_server(Arc::new(|query| vec![reply(query, 0, false, Some(Answer::A([3, 3, 3, 3])))])).await;
    assert_eq!(ask(vec![closed], 300, 1, "a.test").await, Err("ECONNREFUSED"));
    assert_eq!(ask(vec![closed, good], 300, 1, "a.test").await, Ok(json!(["3.3.3.3"])));
    let unassigned = (Ipv4Addr::new(192, 0, 2, 1), Ipv6Addr::UNSPECIFIED);
    let bound = resolver(vec![good], 300, 1);
    let found = bound.resolve("a.test", Kind::A, false, unassigned, CancellationToken::new()).await;
    assert_eq!(found, Err("ECONNREFUSED"));
}

#[test]
fn resolve_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let resolver = resolver(Vec::new(), 1, 1);
    let future = resolver.resolve("a.test", Kind::A, false, ANY, CancellationToken::new());
    assert_send(&future);
}
