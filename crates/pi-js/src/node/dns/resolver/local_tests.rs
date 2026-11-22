//! Local answers for loopback, onion and invalid names, and ordinary queries for nearby names.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use super::super::wire::Kind;
use super::{NO_SERVERS, Resolver};

const ANY: (Ipv4Addr, Ipv6Addr) = (Ipv4Addr::UNSPECIFIED, Ipv6Addr::UNSPECIFIED);
const V6_LOOPBACK: &str = "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.ip6.arpa";

fn resolver(servers: Vec<SocketAddr>) -> Resolver {
    Resolver::new(servers, Duration::from_millis(250), 1, None)
}

async fn ask(resolver: &Resolver, name: &str, kind: Kind, ttl: bool) -> Result<Value, &'static str> {
    resolver.resolve(name, kind, ttl, ANY, CancellationToken::new()).await
}

/// A socket that never answers, and a check that no query reached it.
async fn silent_server() -> (SocketAddr, UdpSocket) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    (socket.local_addr().unwrap(), socket)
}

fn assert_no_packet(socket: &UdpSocket) {
    let mut buffer = [0u8; 512];
    let error = socket.try_recv_from(&mut buffer).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
}

#[tokio::test]
async fn loopback_names_answer_locally_by_record_type() {
    let (server, socket) = silent_server().await;
    for resolver in [resolver(vec![server]), resolver(Vec::new())] {
        for name in ["localhost", "LocalHost.", "a.b.localhost", "ｌｏｃａｌｈｏｓｔ"] {
            assert_eq!(ask(&resolver, name, Kind::A, false).await, Ok(json!(["127.0.0.1"])), "{name}");
            assert_eq!(ask(&resolver, name, Kind::Aaaa, false).await, Ok(json!(["::1"])), "{name}");
            assert_eq!(ask(&resolver, name, Kind::Ptr, false).await, Ok(json!(["localhost"])), "{name}");
            for kind in [Kind::Any, Kind::Cname, Kind::Mx, Kind::Ns, Kind::Soa, Kind::Srv, Kind::Txt, Kind::Caa] {
                assert_eq!(ask(&resolver, name, kind, false).await, Err("ENODATA"), "{name} {kind:?}");
            }
        }
    }
    assert_no_packet(&socket);
}

#[tokio::test]
async fn loopback_ttl_is_one_day() {
    let resolver = resolver(Vec::new());
    let found = ask(&resolver, "localhost", Kind::A, true).await;
    assert_eq!(found, Ok(json!([{ "address": "127.0.0.1", "ttl": 86_400 }])));
    let found = ask(&resolver, "localhost", Kind::Aaaa, true).await;
    assert_eq!(found, Ok(json!([{ "address": "::1", "ttl": 86_400 }])));
    assert_eq!(ask(&resolver, "localhost", Kind::Ptr, true).await, Ok(json!(["localhost"])));
}

#[tokio::test]
async fn loopback_reverse_zones_answer_localhost() {
    let (server, socket) = silent_server().await;
    for resolver in [resolver(vec![server]), resolver(Vec::new())] {
        for name in ["1.0.0.127.in-addr.arpa", "9.8.7.127.IN-ADDR.ARPA.", "127.in-addr.arpa", V6_LOOPBACK] {
            assert_eq!(ask(&resolver, name, Kind::Reverse, false).await, Ok(json!(["localhost"])), "{name}");
            assert_eq!(ask(&resolver, name, Kind::Ptr, false).await, Ok(json!(["localhost"])), "{name}");
            assert_eq!(ask(&resolver, name, Kind::A, false).await, Ok(json!(["127.0.0.1"])), "{name}");
        }
    }
    assert_no_packet(&socket);
}

#[tokio::test]
async fn onion_and_invalid_names_are_not_found_on_label_boundaries() {
    let (server, socket) = silent_server().await;
    for resolver in [resolver(vec![server]), resolver(Vec::new())] {
        for name in ["onion", "ONION.", "a.b.onion", "invalid", "x.Invalid.", "ｘ．ｏｎｉｏｎ"] {
            assert_eq!(resolver.prepare(name, Kind::A).err(), Some("ENOTFOUND"), "{name}");
            assert_eq!(resolver.prepare(name, Kind::Reverse).err(), Some("ENOTFOUND"), "{name}");
        }
    }
    assert_no_packet(&socket);
}

#[tokio::test]
async fn nearby_names_are_ordinary() {
    let nearby = [
        "localhost.com",
        "notlocalhost",
        "a\\.localhost",
        "onion.com",
        "notonion",
        "a\\.onion",
        "invalid.test",
        "128.in-addr.arpa",
        "1.0.0.127.in-addr.arpa.test",
        "in-addr.arpa",
        "2.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.ip6.arpa",
        "0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.ip6.arpa",
    ];
    let empty = resolver(Vec::new());
    let (server, _socket) = silent_server().await;
    let listed = resolver(vec![server]);
    for name in nearby {
        assert_eq!(empty.prepare(name, Kind::A).err(), Some(NO_SERVERS), "{name}");
        let question = listed.prepare(name, Kind::A).unwrap_or_else(|code| panic!("{name}: {code}"));
        assert!(question.local_answer(false).is_none(), "{name}");
    }
}

#[tokio::test]
async fn root_is_a_query_and_failed_conversion_is_a_bad_name() {
    let (server, _socket) = silent_server().await;
    let listed = resolver(vec![server]);
    for name in ["", ".", "。", "．", "｡"] {
        let question = listed.prepare(name, Kind::Ns).unwrap_or_else(|code| panic!("{name:?}: {code}"));
        assert_eq!(&question.encode(0)[12..], [0, 0, 2, 0, 1]);
    }
    for name in ["xn--a.test", "a\u{FFFF}.test", "\u{ad}", "\u{ad}."] {
        assert_eq!(listed.prepare(name, Kind::A).err(), Some("EBADNAME"), "{name:?}");
    }
    let empty = resolver(Vec::new());
    assert_eq!(empty.prepare("", Kind::Ns).err(), Some(NO_SERVERS));
    assert_eq!(empty.prepare("two..dots", Kind::A).err(), Some(NO_SERVERS));
    assert_eq!(empty.prepare("xn--a.test", Kind::A).err(), Some("EBADNAME"));
}
