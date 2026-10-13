//! Agent queue behavior, ported from upstream `agent.test.ts`.

mod support;

use std::sync::Arc;

use futures::FutureExt;
use pi_agent::{Agent, AgentEvent, AgentHooks, AgentInitialState, AgentTurnDecision, QueueMode, StreamFn};
use pi_ai::{Message, StopReason};
use support::*;

fn agent(stream_fn: StreamFn, initial: AgentInitialState) -> Agent {
    let agent = Agent::with_initial_state(AgentInitialState { model: Some(model()), ..initial });
    agent.set_hooks(AgentHooks { stream_fn: Some(stream_fn), ..AgentHooks::default() });
    agent
}

fn user_text(message: &Message) -> Option<String> {
    match message {
        Message::User(user) => Some(user.content.text()),
        _ => None,
    }
}

#[tokio::test]
async fn continue_processes_queued_follow_ups_after_an_assistant_turn() {
    let (_, stream_fn) = scripted(|_, _| text("Processed"));
    let messages = vec![Message::user("Initial"), Message::Assistant(text("Initial response"))];
    let agent = agent(stream_fn, AgentInitialState { messages, ..Default::default() });
    agent.follow_up(Message::user("Queued follow-up"));
    agent.continue_run().await.unwrap();
    let state = agent.state();
    assert!(state.messages.iter().filter_map(user_text).any(|text| text == "Queued follow-up"));
    assert!(matches!(state.messages.last(), Some(Message::Assistant(_))));
}

#[tokio::test]
async fn continue_keeps_steering_mode_semantics_for_an_assistant_tail() {
    for (mode, expected_requests) in [(QueueMode::OneAtATime, 2), (QueueMode::All, 1)] {
        let (requests, stream_fn) = scripted(|_, _| text("Processed"));
        let messages = vec![Message::user("Initial"), Message::Assistant(text("Initial response"))];
        let agent = agent(stream_fn, AgentInitialState { messages, ..Default::default() });
        agent.set_queue_modes(mode, QueueMode::OneAtATime);
        agent.steer(Message::user("Steering 1"));
        agent.steer(Message::user("Steering 2"));
        agent.continue_run().await.unwrap();
        let requests: Vec<Vec<String>> =
            requests.lock().unwrap().iter().map(|request| user_texts(&request.context)).collect();
        assert_eq!(requests.len(), expected_requests, "{mode:?}");
        assert!(requests[0].contains(&"Steering 1".to_string()));
        let second_in_first = requests[0].contains(&"Steering 2".to_string());
        assert_eq!(second_in_first, mode == QueueMode::All);
    }
}

#[tokio::test]
async fn keeps_steering_ahead_of_follow_ups_from_a_user_tail() {
    let (requests, stream_fn) = scripted(|_, _| text("done"));
    let agent = agent(stream_fn, AgentInitialState { messages: vec![Message::user("existing")], ..Default::default() });
    agent.steer(Message::user("steering"));
    agent.follow_up(Message::user("follow-up"));
    agent.continue_run().await.unwrap();
    let requests: Vec<Vec<String>> =
        requests.lock().unwrap().iter().map(|request| user_texts(&request.context)).collect();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains(&"steering".to_string()) && !requests[0].contains(&"follow-up".to_string()));
    assert!(requests[1].contains(&"follow-up".to_string()));
}

#[tokio::test]
async fn keeps_queues_when_a_run_ends_early() {
    let failing = |reason: StopReason| -> StreamFn {
        scripted(move |_, _| {
            let mut message = assistant(vec![], reason);
            message.error_message = Some("failed".into());
            message
        })
        .1
    };
    let cases: Vec<(StreamFn, AgentTurnDecision)> = vec![
        (failing(StopReason::Error), AgentTurnDecision::Continue),
        (failing(StopReason::Aborted), AgentTurnDecision::Continue),
        (scripted(|_, _| text("done")).1, AgentTurnDecision::End),
    ];
    for (stream_fn, decision) in cases {
        let agent = agent(stream_fn, AgentInitialState::default());
        agent.update_hooks(|hooks| {
            hooks.finish_turn = Some(Arc::new(move |_, _| async move { Some(decision) }.boxed()))
        });
        agent.follow_up(Message::user("follow-up"));
        let steering_agent = agent.clone();
        agent.subscribe(move |event| {
            if let AgentEvent::MessageEnd { message: Message::Assistant(_) } = event {
                steering_agent.steer(Message::user("steering"));
            }
        });
        agent.prompt("start", vec![]).await.unwrap();
        let peeked: Vec<String> = agent.peek_queued_messages().iter().filter_map(user_text).collect();
        assert_eq!(peeked, ["steering"]);
        agent.clear_steering_queue();
        let peeked: Vec<String> = agent.peek_queued_messages().iter().filter_map(user_text).collect();
        assert_eq!(peeked, ["follow-up"]);
    }
}

#[test]
fn peeks_the_next_selected_queued_messages_without_consuming_them() {
    let agent = Agent::new(model());
    agent.set_queue_modes(QueueMode::OneAtATime, QueueMode::All);
    agent.steer(Message::user("first steering"));
    agent.steer(Message::user("second steering"));
    agent.follow_up(Message::user("follow-up"));
    let peek = |agent: &Agent| agent.peek_queued_messages().iter().filter_map(user_text).collect::<Vec<_>>();
    assert_eq!(peek(&agent), ["first steering"]);
    assert_eq!(peek(&agent), ["first steering"]);
    agent.clear_steering_queue();
    assert_eq!(peek(&agent), ["follow-up"]);
    assert_eq!((agent.steering_mode(), agent.follow_up_mode()), (QueueMode::OneAtATime, QueueMode::All));
}
