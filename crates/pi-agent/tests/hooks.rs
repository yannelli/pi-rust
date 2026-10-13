//! `finish_turn`, `prepare_request`, and `prepare_next_turn`, ported from upstream `agent-loop.test.ts`.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use pi_agent::{
    AgentContext, AgentEvent, AgentLoopTurnUpdate, AgentRequestUpdate, AgentTurnDecision, MessageSource, run_agent_loop,
};
use pi_ai::{ContentBlock, Message, StopReason, SystemMessage, ThinkingLevel, UserContent};
use serde_json::json;
use support::*;
use tokio_util::sync::CancellationToken;

fn counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

fn source(polls: Arc<AtomicUsize>, respond: impl Fn(usize) -> Vec<Message> + Send + Sync + 'static) -> MessageSource {
    let respond = Arc::new(respond);
    Arc::new(move || {
        let poll = polls.fetch_add(1, Ordering::SeqCst) + 1;
        let messages = respond(poll);
        async move { messages }.boxed()
    })
}

#[tokio::test]
async fn finish_turn_runs_after_tool_results_and_before_turn_end() {
    let (_, stream_fn) = sequence(vec![tool_use(vec![call("tool-1", "echo", json!({"value": "hello"}))])]);
    let tool = support::FnTool::new("echo", json!({"type": "object"}), |_, _| {
        Ok(pi_agent::AgentToolResult { terminate: true, ..pi_agent::AgentToolResult::text("hello") })
    });
    let ordering = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut config = config(stream_fn);
    let seen = ordering.clone();
    config.finish_turn = Some(Arc::new(move |turn, _| {
        assert_eq!(turn.tool_results.len(), 1);
        assert!(matches!(turn.context.messages.last(), Some(Message::ToolResult(_))));
        seen.lock().unwrap().push("finishTurn".into());
        async { None }.boxed()
    }));
    let seen = ordering.clone();
    let sink: pi_agent::EventSink = Arc::new(move |event| match &event {
        AgentEvent::MessageEnd { message } => seen.lock().unwrap().push(format!("message_end:{}", role(message))),
        AgentEvent::TurnEnd { .. } => seen.lock().unwrap().push("turn_end".into()),
        _ => {}
    });
    let mut context = AgentContext { system_prompt: Some(String::new()), tools: vec![tool], ..Default::default() };
    run_agent_loop(vec![Message::user("echo")], &mut context, &config, sink, CancellationToken::new()).await;
    let ordering = ordering.lock().unwrap();
    assert_eq!(ordering[ordering.len() - 3..], ["message_end:toolResult", "finishTurn", "turn_end"]);
}

#[tokio::test]
async fn finish_turn_on_failed_responses_keeps_the_hard_exit() {
    for reason in [StopReason::Error, StopReason::Aborted] {
        let (requests, stream_fn) = scripted(move |_, _| {
            let mut message = assistant(vec![], reason);
            message.error_message = Some("failed".into());
            message
        });
        let (steering, follow_ups) = (counter(), counter());
        let mut config = config(stream_fn);
        config.get_steering_messages = Some(source(steering.clone(), |_| Vec::new()));
        config.get_follow_up_messages = Some(source(follow_ups.clone(), |_| vec![Message::user("queued")]));
        let finished = counter();
        let calls = finished.clone();
        config.finish_turn = Some(Arc::new(move |turn, _| {
            assert_eq!(turn.message.stop_reason, reason);
            calls.fetch_add(1, Ordering::SeqCst);
            async { Some(AgentTurnDecision::Continue) }.boxed()
        }));
        let (events, sink) = recorder();
        run_agent_loop(
            vec![Message::user("run")],
            &mut AgentContext::default(),
            &config,
            sink,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(finished.load(Ordering::SeqCst), 1);
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!((steering.load(Ordering::SeqCst), follow_ups.load(Ordering::SeqCst)), (1, 0));
        let kinds = kinds(&events.lock().unwrap());
        assert_eq!(kinds[kinds.len() - 2..], ["turn_end", "agent_end"]);
    }
}

#[tokio::test]
async fn end_decision_skips_queue_polling_and_next_turn_preparation() {
    let (requests, stream_fn) = sequence(vec![tool_use(vec![call("tool-1", "noop", json!({}))])]);
    let (steering, follow_ups, prepares) = (counter(), counter(), counter());
    let mut config = config(stream_fn);
    config.get_steering_messages = Some(source(steering.clone(), |_| Vec::new()));
    config.get_follow_up_messages = Some(source(follow_ups.clone(), |_| vec![Message::user("queued")]));
    let tool_results = Arc::new(Mutex::new(Vec::new()));
    let context_roles = Arc::new(Mutex::new(Vec::new()));
    let (seen, seen_roles) = (tool_results.clone(), context_roles.clone());
    config.finish_turn = Some(Arc::new(move |turn, _| {
        seen.lock().unwrap().extend(turn.tool_results.iter().map(|result| result.tool_call_id.clone()));
        *seen_roles.lock().unwrap() = roles(&turn.context.messages);
        async { Some(AgentTurnDecision::End) }.boxed()
    }));
    let prepare_calls = prepares.clone();
    config.prepare_next_turn = Some(Arc::new(move |_, _| {
        prepare_calls.fetch_add(1, Ordering::SeqCst);
        async { None }.boxed()
    }));
    let mut context = AgentContext { tools: vec![FnTool::noop()], ..Default::default() };
    let (events, sink) = recorder();
    let added = run_agent_loop(vec![Message::user("run")], &mut context, &config, sink, CancellationToken::new()).await;
    // The context declares no tools, so the loop announces the loadout with a system message.
    assert_eq!(roles(&added), ["system", "user", "assistant", "toolResult"]);
    assert_eq!(*context_roles.lock().unwrap(), ["system", "user", "assistant", "toolResult"]);
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(steering.load(Ordering::SeqCst), 1);
    assert_eq!(follow_ups.load(Ordering::SeqCst), 0);
    assert_eq!(prepares.load(Ordering::SeqCst), 0);
    assert_eq!(*tool_results.lock().unwrap(), ["tool-1"]);
    let kinds = kinds(&events.lock().unwrap());
    assert_eq!(
        kinds[kinds.len() - 5..],
        ["tool_execution_end", "message_start", "message_end", "turn_end", "agent_end"]
    );
}

#[tokio::test]
async fn continue_decision_makes_one_context_only_request() {
    let (requests, stream_fn) = scripted(|index, _| text(&format!("response {}", index + 1)));
    let finished = counter();
    let mut config = config(stream_fn);
    let calls = finished.clone();
    config.finish_turn = Some(Arc::new(move |_, _| {
        let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
        async move { first.then_some(AgentTurnDecision::Continue) }.boxed()
    }));
    let (_, sink) = recorder();
    run_agent_loop(vec![Message::user("run")], &mut AgentContext::default(), &config, sink, CancellationToken::new())
        .await;
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert_eq!(finished.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn natural_requests_satisfy_continue() {
    // A tool-result request.
    let (requests, stream_fn) = sequence(vec![tool_use(vec![call("tool-1", "noop", json!({}))])]);
    let finished = counter();
    let mut tool_config = config(stream_fn);
    let calls = finished.clone();
    tool_config.finish_turn = Some(Arc::new(move |_, _| {
        let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
        async move { first.then_some(AgentTurnDecision::Continue) }.boxed()
    }));
    let mut context = AgentContext { tools: vec![FnTool::noop()], ..Default::default() };
    let (_, sink) = recorder();
    run_agent_loop(vec![Message::user("run")], &mut context, &tool_config, sink, CancellationToken::new()).await;
    assert_eq!((requests.lock().unwrap().len(), finished.load(Ordering::SeqCst)), (2, 2));

    // Steering and follow-up requests.
    for steering_kind in [true, false] {
        let (requests, stream_fn) = scripted(|_, _| text("done"));
        let finished = counter();
        let mut config = config(stream_fn);
        let calls = finished.clone();
        config.finish_turn = Some(Arc::new(move |_, _| {
            let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
            async move { first.then_some(AgentTurnDecision::Continue) }.boxed()
        }));
        config.get_steering_messages = Some(source(counter(), move |poll| {
            if steering_kind && poll == 2 { vec![Message::user("queued")] } else { Vec::new() }
        }));
        config.get_follow_up_messages = Some(source(counter(), move |poll| {
            if !steering_kind && poll == 1 { vec![Message::user("queued")] } else { Vec::new() }
        }));
        let (_, sink) = recorder();
        run_agent_loop(
            vec![Message::user("run")],
            &mut AgentContext::default(),
            &config,
            sink,
            CancellationToken::new(),
        )
        .await;
        let requests = requests.lock().unwrap();
        assert_eq!((requests.len(), finished.load(Ordering::SeqCst)), (2, 2));
        assert!(user_texts(&requests[1].context).contains(&"queued".to_string()));
    }
}

#[tokio::test]
async fn prepare_request_runs_after_pending_messages_and_replaces_request_state() {
    let (requests, stream_fn) = scripted(|_, _| text("done"));
    let mut replacement = model();
    replacement.id = "replacement".into();
    let mut config = config(stream_fn);
    config.get_steering_messages =
        Some(source(counter(), |poll| if poll == 1 { vec![Message::user("steering")] } else { Vec::new() }));
    let prepares = counter();
    let calls = prepares.clone();
    let next_model = replacement.clone();
    config.prepare_request = Some(Arc::new(move |request, _| {
        calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            request
                .context
                .messages
                .iter()
                .any(|message| matches!(message, Message::User(user) if user.content.text() == "steering"))
        );
        let update = AgentRequestUpdate {
            context: Some(AgentContext {
                messages: vec![Message::user("canonical projection")],
                ..request.context.clone()
            }),
            model: Some(next_model.clone()),
            thinking_level: Some(ThinkingLevel::High),
        };
        async move { Some(update) }.boxed()
    }));
    let (_, sink) = recorder();
    run_agent_loop(
        vec![Message::user("prompt")],
        &mut AgentContext::default(),
        &config,
        sink,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(prepares.load(Ordering::SeqCst), 1);
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0].model.id, "replacement");
    assert_eq!(user_texts(&requests[0].context), ["canonical projection"]);
    assert_eq!(requests[0].reasoning, Some(ThinkingLevel::High));
}

#[tokio::test]
async fn prepare_request_does_not_poll_steering() {
    let queued = Arc::new(Mutex::new(Vec::<Message>::new()));
    let (requests, stream_fn) = scripted(|_, _| text("done"));
    let mut config = config(stream_fn);
    let steering = counter();
    let queue = queued.clone();
    config.get_steering_messages = Some(source(steering.clone(), move |_| queue.lock().unwrap().drain(..).collect()));
    let prepares = counter();
    let (calls, queue) = (prepares.clone(), queued.clone());
    config.prepare_request = Some(Arc::new(move |_, _| {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            queue.lock().unwrap().push(Message::user("late steering"));
        }
        async { None }.boxed()
    }));
    let (_, sink) = recorder();
    run_agent_loop(vec![Message::user("run")], &mut AgentContext::default(), &config, sink, CancellationToken::new())
        .await;
    let included: Vec<bool> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| user_texts(&request.context).contains(&"late steering".to_string()))
        .collect();
    assert_eq!(included, [false, true]);
    assert_eq!(prepares.load(Ordering::SeqCst), 2);
    // Startup, post-turn delivery, then the final natural-stop check.
    assert_eq!(steering.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn prepare_next_turn_replaces_context_and_appends_messages() {
    let (requests, stream_fn) = sequence(vec![tool_use(vec![call("tool-1", "echo", json!({"value": "hello"}))])]);
    let prepares = counter();
    let mut config = config(stream_fn);
    config.reasoning = Some(ThinkingLevel::Low);
    let calls = prepares.clone();
    config.prepare_next_turn = Some(Arc::new(move |turn, _| {
        let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
        let update = first.then(|| AgentLoopTurnUpdate {
            context: Some(turn.context.clone()),
            messages: vec![Message::System(SystemMessage {
                content: UserContent::Text("updated guidance".into()),
                sections: None,
                tools_added: None,
                tools_removed: None,
                timestamp: 1,
            })],
            model: None,
            thinking_level: Some(ThinkingLevel::Off),
        });
        async move { update }.boxed()
    }));
    let mut context =
        AgentContext { system_prompt: Some("p".into()), tools: vec![FnTool::echo()], ..Default::default() };
    let (_, sink) = recorder();
    run_agent_loop(vec![Message::user("echo something")], &mut context, &config, sink, CancellationToken::new()).await;
    assert_eq!(prepares.load(Ordering::SeqCst), 1);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let has_update = requests[1]
        .context
        .messages
        .iter()
        .any(|message| matches!(message, Message::System(system) if system.content.text() == "updated guidance"));
    assert!(has_update);
    assert_eq!((requests[0].reasoning, requests[1].reasoning), (Some(ThinkingLevel::Low), None));
    let Some(Message::Assistant(last)) = context.messages.last() else { panic!("expected assistant") };
    assert_eq!(last.thinking_level, Some(ThinkingLevel::Off));
}

#[tokio::test]
async fn steering_queued_during_prepare_next_turn_reaches_the_next_request() {
    let queued = Arc::new(Mutex::new(Vec::<Message>::new()));
    let (requests, stream_fn) = sequence(vec![tool_use(vec![call("tool-1", "noop", json!({}))])]);
    let mut config = config(stream_fn);
    let queue = queued.clone();
    config.prepare_next_turn = Some(Arc::new(move |_, _| {
        queue.lock().unwrap().push(Message::user("late steering"));
        async { None }.boxed()
    }));
    let queue = queued.clone();
    config.get_steering_messages = Some(source(counter(), move |_| queue.lock().unwrap().drain(..).collect()));
    let mut context = AgentContext { tools: vec![FnTool::noop()], ..Default::default() };
    let (_, sink) = recorder();
    run_agent_loop(vec![Message::user("run")], &mut context, &config, sink, CancellationToken::new()).await;
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(user_texts(&requests[1].context).contains(&"late steering".to_string()));
}

#[tokio::test]
async fn tool_results_carry_usage_and_after_hooks_replace_structured_content() {
    let usage = pi_ai::Usage { input: 3, output: 4, total_tokens: 7, ..Default::default() };
    let tool = FnTool::new("structured", json!({"type": "object"}), move |_, _| {
        Ok(pi_agent::AgentToolResult::text("raw").with_structured_content(json!({"n": 1})).with_usage(usage))
    });
    let (_, stream_fn) = sequence(vec![tool_use(vec![call("a", "structured", json!({}))])]);
    let mut config = config(stream_fn);
    config.after_tool_call = Some(Arc::new(|input, _| {
        assert_eq!(input.result.structured_content, Some(json!({"n": 1})));
        assert_eq!(input.context.tools.len(), 1);
        async {
            Some(pi_agent::AfterToolCallResult { content: Some(vec![ContentBlock::text("new")]), ..Default::default() })
        }
        .boxed()
    }));
    let mut context = AgentContext { tools: vec![tool], ..Default::default() };
    let (events, sink) = recorder();
    let added = run_agent_loop(vec![Message::user("go")], &mut context, &config, sink, CancellationToken::new()).await;
    let result = added.iter().find_map(|message| match message {
        Message::ToolResult(result) => Some(result.clone()),
        _ => None,
    });
    let result = result.expect("tool result");
    assert_eq!(result.usage.map(|usage| usage.total_tokens), Some(7));
    assert_eq!(result.content[0].as_text(), Some("new"));
    let end = events.lock().unwrap().iter().find_map(|event| match event {
        AgentEvent::ToolExecutionEnd { result, .. } => Some(result.clone()),
        _ => None,
    });
    assert_eq!(end.expect("tool_execution_end").structured_content, None);
}

#[tokio::test]
async fn base_stream_options_reach_the_provider_under_loop_settings() {
    let (requests, stream_fn) = sequence(vec![]);
    let mut config = config(stream_fn);
    config.session_id = Some("loop-session".into());
    config.stream_options = pi_ai::StreamOptions {
        transport: Some(pi_ai::Transport::Sse),
        max_tokens: Some(50),
        session_id: Some("base-session".into()),
        ..Default::default()
    };
    let (_, sink) = recorder();
    run_agent_loop(vec![Message::user("run")], &mut AgentContext::default(), &config, sink, CancellationToken::new())
        .await;
    let request = requests.lock().unwrap()[0].clone();
    assert_eq!(request.transport, Some(pi_ai::Transport::Sse));
    assert_eq!((request.max_tokens, request.session_id.as_deref()), (Some(50), Some("loop-session")));
}
