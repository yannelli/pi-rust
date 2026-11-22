//! The UDP receive loop against an always-readable socket mock, in the `select!` shapes of
//! `Attempt::exchange` and `Resolver::resolve_question`. No datagram leaves the process.

use std::future::Future;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bytes::BytesMut;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::super::super::wire::{Kind, Question, Reply};
use super::super::timer::Timer;
use super::super::{CANCELLED, TIMEOUT};
use super::{Attempt, Datagrams, UNRELATED_PER_YIELD};

const ID: u16 = 0x1234;
/// Receives allowed before the mock fails the test; a loop that never yields reaches it.
const LIMIT: usize = 100 * UNRELATED_PER_YIELD;

/// Always readable. Receive `n` (from 1) gets the answer when `script(n)` is true, else a
/// datagram with another ID.
struct Flood {
    answer: Vec<u8>,
    unrelated: Vec<u8>,
    script: Box<dyn Fn(usize) -> bool>,
    received: AtomicUsize,
}

impl Flood {
    fn new(question: &Question, script: impl Fn(usize) -> bool + 'static) -> Self {
        let mut answer = question.encode(ID);
        answer[2..4].copy_from_slice(&[0x81, 0x80]);
        answer[7] = 1;
        answer.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 1, 2, 3, 4]);
        let mut unrelated = answer.clone();
        unrelated[1] ^= 1;
        Self { answer, unrelated, script: Box::new(script), received: AtomicUsize::new(0) }
    }

    fn received(&self) -> usize {
        self.received.load(Ordering::SeqCst)
    }
}

impl Datagrams for Flood {
    fn wait_readable(&self) -> impl Future<Output = std::io::Result<()>> + Send {
        std::future::ready(Ok(()))
    }

    fn receive_into(&self, buffer: &mut BytesMut) -> std::io::Result<usize> {
        let count = self.received.fetch_add(1, Ordering::SeqCst) + 1;
        assert!(count <= LIMIT, "the receive loop did not yield in {LIMIT} unrelated datagrams");
        let datagram = if (self.script)(count) { &self.answer } else { &self.unrelated };
        buffer.extend_from_slice(datagram);
        Ok(datagram.len())
    }
}

fn question() -> Question {
    Question::new("a.test", Kind::A).expect("valid name")
}

fn attempt<'a>(question: &'a Question, packet: &'a [u8]) -> Attempt<'a> {
    let server = SocketAddr::from(([127, 0, 0, 1], 53));
    Attempt::new(question, packet, ID, false, server, (Ipv4Addr::UNSPECIFIED, Ipv6Addr::UNSPECIFIED))
}

fn answered() -> Result<Reply, &'static str> {
    Ok(Reply::Answer { value: json!(["1.2.3.4"]), measure: true })
}

#[tokio::test]
async fn one_poll_reads_a_bounded_number_of_unrelated_datagrams() {
    let (question, timer) = (question(), Timer::new(Duration::from_secs(1)));
    let (packet, running) = (question.encode(ID), timer.enter());
    let attempt = attempt(&question, &packet);
    let mut context = Context::from_waker(Waker::noop());

    let endless = Flood::new(&question, |_| false);
    let mut receive = std::pin::pin!(attempt.receive(&endless, &running));
    assert!(receive.as_mut().poll(&mut context).is_pending());
    assert_eq!(endless.received(), UNRELATED_PER_YIELD);

    let late = Flood::new(&question, |count| count == UNRELATED_PER_YIELD);
    let mut receive = std::pin::pin!(attempt.receive(&late, &running));
    assert_eq!(receive.as_mut().poll(&mut context), Poll::Ready(answered()));

    let first = Flood::new(&question, |_| true);
    let mut receive = std::pin::pin!(attempt.receive(&first, &running));
    assert_eq!(receive.as_mut().poll(&mut context), Poll::Ready(answered()));
    assert_eq!(first.received(), 1);
}

#[tokio::test]
async fn an_always_readable_socket_still_times_out() {
    let (question, timer) = (question(), Timer::new(Duration::from_secs(1)));
    let (packet, running) = (question.encode(ID), timer.enter());
    let flood = Flood::new(&question, |_| false);
    let attempt = attempt(&question, &packet);
    // Before the runtime's timer start, so the wheel fires it on its first poll instead of on
    // the next millisecond tick.
    let deadline = tokio::time::Instant::now() - Duration::from_secs(1);
    let result = tokio::select! {
        biased;
        result = attempt.receive(&flood, &running) => result,
        () = running.expired(deadline) => Err(TIMEOUT),
    };
    assert_eq!(result, Err(TIMEOUT));
    assert_eq!(flood.received(), UNRELATED_PER_YIELD);
}

#[tokio::test]
async fn an_always_readable_socket_still_cancels() {
    let (question, timer) = (question(), Timer::new(Duration::from_secs(1)));
    let (packet, running) = (question.encode(ID), timer.enter());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let flood = Flood::new(&question, move |_| {
        trigger.cancel();
        false
    });
    let attempt = attempt(&question, &packet);
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(CANCELLED),
        result = attempt.receive(&flood, &running) => result,
    };
    assert_eq!(result, Err(CANCELLED));
    assert_eq!(flood.received(), UNRELATED_PER_YIELD);
}

#[tokio::test]
async fn an_always_readable_socket_lets_other_tasks_on_the_worker_run() {
    let (question, timer) = (question(), Timer::new(Duration::from_secs(1)));
    let (packet, running) = (question.encode(ID), timer.enter());
    let sibling_ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&sibling_ran);
    let sibling = tokio::spawn(async move { flag.store(true, Ordering::SeqCst) });
    let seen = Arc::clone(&sibling_ran);
    let flood = Flood::new(&question, move |_| seen.load(Ordering::SeqCst));
    assert_eq!(attempt(&question, &packet).receive(&flood, &running).await, answered());
    assert!(flood.received() > UNRELATED_PER_YIELD);
    sibling.await.expect("sibling task");
}
