//! Async subscribers and the idle barrier, ported from upstream `agent.test.ts`.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_agent::{Agent, AgentEvent, AgentHooks, AgentInitialState, StreamFn};
use pi_ai::stream::event_channel;
use pi_ai::{AssistantMessageEvent, Message, StopReason, StreamOptions};
use support::*;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn agent(stream_fn: StreamFn) -> Agent {
    let agent = Agent::with_initial_state(AgentInitialState { model: Some(model()), ..Default::default() });
    agent.set_hooks(AgentHooks { stream_fn: Some(stream_fn), ..AgentHooks::default() });
    agent
}

fn ok_agent() -> Agent {
    agent(scripted(|_, _| text("ok")).1)
}

#[tokio::test]
async fn awaits_async_subscribers_before_prompt_resolves() {
    let agent = ok_agent();
    let barrier = Arc::new(Notify::new());
    let finished = Arc::new(AtomicBool::new(false));
    let (wait, done) = (barrier.clone(), finished.clone());
    agent.subscribe_async(move |event, _signal| {
        let (wait, done) = (wait.clone(), done.clone());
        async move {
            if matches!(event, AgentEvent::AgentEnd { .. }) {
                wait.notified().await;
                done.store(true, Ordering::SeqCst);
            }
        }
    });

    let running = agent.clone();
    let prompt = tokio::spawn(async move { running.prompt("hello", Vec::new()).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!prompt.is_finished());
    assert!(!finished.load(Ordering::SeqCst));
    assert!(agent.state().is_streaming);

    barrier.notify_one();
    prompt.await.unwrap().unwrap();
    assert!(finished.load(Ordering::SeqCst));
    assert!(!agent.state().is_streaming);
}

#[tokio::test]
async fn wait_for_idle_waits_for_async_subscribers() {
    let agent = ok_agent();
    let barrier = Arc::new(Notify::new());
    let wait = barrier.clone();
    agent.subscribe_async(move |event, _signal| {
        let wait = wait.clone();
        async move {
            if matches!(&event, AgentEvent::MessageEnd { message: Message::Assistant(_) }) {
                wait.notified().await;
            }
        }
    });

    let running = agent.clone();
    let prompt = tokio::spawn(async move { running.prompt("hello", Vec::new()).await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    let idle_agent = agent.clone();
    let idle = tokio::spawn(async move { idle_agent.wait_for_idle().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!idle.is_finished());
    assert!(agent.state().is_streaming);

    barrier.notify_one();
    prompt.await.unwrap().unwrap();
    idle.await.unwrap();
    assert!(!agent.state().is_streaming);
}

#[tokio::test]
async fn async_subscribers_get_events_in_order_and_sync_listeners_still_work() {
    let agent = ok_agent();
    let sync_seen = Arc::new(Mutex::new(Vec::new()));
    let sink = sync_seen.clone();
    agent.subscribe(move |event| sink.lock().unwrap().push(kind(event)));
    let async_seen = Arc::new(Mutex::new(Vec::new()));
    let sink = async_seen.clone();
    let id = agent.subscribe_async(move |event, _signal| {
        let sink = sink.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            sink.lock().unwrap().push(kind(&event));
        }
    });
    agent.prompt("hello", Vec::new()).await.unwrap();
    let expected = sync_seen.lock().unwrap().clone();
    assert_eq!(*async_seen.lock().unwrap(), expected);
    assert_eq!(expected.last().map(String::as_str), Some("agent_end"));

    agent.unsubscribe(id);
    async_seen.lock().unwrap().clear();
    agent.prompt("again", Vec::new()).await.unwrap();
    assert!(async_seen.lock().unwrap().is_empty());
}

/// A stream that starts, then ends as aborted once the request is cancelled.
fn abortable() -> StreamFn {
    Arc::new(|model, _context, options: StreamOptions| {
        let (sender, stream) = event_channel();
        let partial = assistant(Vec::new(), StopReason::Stop);
        sender.push(AssistantMessageEvent::Start { partial: partial.clone() });
        let mut error = pi_ai::AssistantMessage { stop_reason: StopReason::Aborted, ..partial };
        error.error_message = Some("Aborted".into());
        error.model = model.id.clone();
        tokio::spawn(async move {
            options.cancel.cancelled().await;
            sender.push(AssistantMessageEvent::Error { reason: StopReason::Aborted, error });
        });
        stream
    })
}

#[tokio::test]
async fn passes_the_active_abort_signal_to_subscribers() {
    let agent = agent(abortable());
    let received: Arc<Mutex<Option<CancellationToken>>> = Arc::default();
    let sink = received.clone();
    agent.subscribe_async(move |event, signal| {
        if matches!(event, AgentEvent::AgentStart) {
            *sink.lock().unwrap() = Some(signal);
        }
        async {}
    });

    let running = agent.clone();
    let prompt = tokio::spawn(async move { running.prompt("hello", Vec::new()).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let signal = received.lock().unwrap().clone().expect("signal");
    assert!(!signal.is_cancelled());

    agent.abort();
    prompt.await.unwrap().unwrap();
    assert!(signal.is_cancelled());
}
