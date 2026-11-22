//! Wire ID ownership on a shared stream. Each case holds one sibling question open so the
//! stream stays up while a ticket is withdrawn, answered, or replaced.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::super::super::wire::{Kind, Question, Reply};
use super::super::transport::Attempt;
use super::{TcpPool, Ticket};

const ANY: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

/// Fails a case that would otherwise hang on a lost question or frame.
async fn soon<T>(step: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), step).await.expect("step finished within 5 s")
}

struct Asked {
    question: Question,
    packet: Vec<u8>,
    id: u16,
}

fn asked(name: &str, id: u16) -> Asked {
    let question = Question::new(name, Kind::A).unwrap();
    let packet = question.encode(id);
    Asked { question, packet, id }
}

impl Asked {
    fn send(&self, pool: &TcpPool, server: SocketAddr) -> Ticket {
        let attempt =
            Attempt { question: &self.question, packet: &self.packet, id: self.id, ttl: false, server, source: ANY };
        pool.send(&attempt).unwrap()
    }
}

fn framed(body: &[u8]) -> Vec<u8> {
    let mut frame = u16::try_from(body.len()).unwrap().to_be_bytes().to_vec();
    frame.extend_from_slice(body);
    frame
}

/// A NOERROR reply to `query`, as read from the wire, with one A record `192.0.2.<last>`.
fn reply(query: &[u8], last: u8) -> Vec<u8> {
    let mut body = query.to_vec();
    body[2] |= 0x80;
    body[7] = 1;
    body.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 192, 0, 2, last]);
    framed(&body)
}

fn address(last: u8) -> Result<Reply, &'static str> {
    Ok(Reply::Answer { value: json!([format!("192.0.2.{last}")]), measure: true })
}

async fn read_query(stream: &mut TcpStream) -> Vec<u8> {
    let mut query = vec![0; usize::from(soon(stream.read_u16()).await.unwrap())];
    soon(stream.read_exact(&mut query)).await.unwrap();
    query
}

fn wire_id(query: &[u8]) -> u16 {
    u16::from_be_bytes([query[0], query[1]])
}

async fn listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    (listener, address)
}

async fn assert_closed(stream: &mut TcpStream) {
    assert_eq!(soon(stream.read(&mut [0])).await.unwrap(), 0, "the stream closes after its last question");
}

#[tokio::test]
async fn dropping_an_answered_ticket_keeps_the_question_that_reused_its_id() {
    let (listener, server) = listener().await;
    let pool = TcpPool::default();
    let (sibling, first, second) = (asked("sibling.test", 1), asked("a.test", 7), asked("b.test", 7));
    let mut sibling_ticket = sibling.send(&pool, server);
    let mut first_ticket = first.send(&pool, server);
    let (mut stream, _) = soon(listener.accept()).await.unwrap();
    let sibling_query = read_query(&mut stream).await;
    let first_query = read_query(&mut stream).await;
    stream.write_all(&reply(&first_query, 1)).await.unwrap();
    assert_eq!(soon(first_ticket.reply()).await, address(1));
    let mut second_ticket = second.send(&pool, server);
    drop(first_ticket);
    let second_query = read_query(&mut stream).await;
    assert_eq!(wire_id(&second_query), 7, "an answered ID is free again");
    stream.write_all(&reply(&second_query, 2)).await.unwrap();
    assert_eq!(soon(second_ticket.reply()).await, address(2));
    stream.write_all(&reply(&sibling_query, 3)).await.unwrap();
    assert_eq!(soon(sibling_ticket.reply()).await, address(3));
    assert_closed(&mut stream).await;
}

#[tokio::test]
async fn a_late_reply_to_a_withdrawn_question_does_not_reach_its_successor() {
    let (listener, server) = listener().await;
    let pool = TcpPool::default();
    let (sibling, same) = (asked("sibling.test", 1), asked("same.test", 9));
    let mut sibling_ticket = sibling.send(&pool, server);
    let withdrawn = same.send(&pool, server);
    let (mut stream, _) = soon(listener.accept()).await.unwrap();
    let sibling_query = read_query(&mut stream).await;
    let withdrawn_query = read_query(&mut stream).await;
    drop(withdrawn);
    let mut successor = same.send(&pool, server);
    let successor_query = read_query(&mut stream).await;
    assert_eq!((wire_id(&withdrawn_query), wire_id(&successor_query)), (9, 10), "a withdrawn ID is retired");
    let mut late = reply(&withdrawn_query, 1);
    late.extend(reply(&successor_query, 2));
    stream.write_all(&late).await.unwrap();
    assert_eq!(soon(successor.reply()).await, address(2));
    stream.write_all(&reply(&sibling_query, 3)).await.unwrap();
    assert_eq!(soon(sibling_ticket.reply()).await, address(3));
    assert_closed(&mut stream).await;
}

#[tokio::test]
async fn a_question_withdrawn_before_its_write_is_not_sent() {
    let (listener, server) = listener().await;
    let pool = TcpPool::default();
    let (sibling, gone) = (asked("sibling.test", 1), asked("gone.test", 5));
    let mut sibling_ticket = sibling.send(&pool, server);
    drop(gone.send(&pool, server));
    let (mut stream, _) = soon(listener.accept()).await.unwrap();
    let sibling_query = read_query(&mut stream).await;
    stream.write_all(&reply(&sibling_query, 3)).await.unwrap();
    assert_eq!(soon(sibling_ticket.reply()).await, address(3));
    assert_closed(&mut stream).await;
}

#[tokio::test]
async fn a_malformed_frame_with_an_unknown_id_fails_the_stream() {
    let (listener, server) = listener().await;
    let pool = TcpPool::default();
    let sibling = asked("sibling.test", 1);
    let mut sibling_ticket = sibling.send(&pool, server);
    let (mut stream, _) = soon(listener.accept()).await.unwrap();
    let mut body = read_query(&mut stream).await;
    body[0] ^= 0xFF;
    body[2] |= 0x80;
    body[7] = 1;
    stream.write_all(&framed(&body)).await.unwrap();
    assert_eq!(soon(sibling_ticket.reply()).await, Err("EBADRESP"));
    assert_closed(&mut stream).await;
}

#[tokio::test]
async fn exhausting_wire_ids_opens_another_stream_while_the_first_finishes() {
    let (listener, server) = listener().await;
    let pool = TcpPool::default();
    let mut first = asked("first.test", 0).send(&pool, server);
    let (mut old, _) = soon(listener.accept()).await.unwrap();
    let first_query = read_query(&mut old).await;
    {
        let mut state = super::lock(&pool.shared);
        state.links.get_mut(&(ANY, server)).unwrap().retired.extend(1..=u16::MAX);
    }
    let mut second = asked("second.test", 0).send(&pool, server);
    let (mut new, _) = soon(listener.accept()).await.unwrap();
    let second_query = read_query(&mut new).await;
    assert_eq!(wire_id(&second_query), 0);
    old.write_all(&reply(&first_query, 1)).await.unwrap();
    assert_eq!(soon(first.reply()).await, address(1));
    assert_closed(&mut old).await;
    new.write_all(&reply(&second_query, 2)).await.unwrap();
    assert_eq!(soon(second.reply()).await, address(2));
    assert_closed(&mut new).await;
}
