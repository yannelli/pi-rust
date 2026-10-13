//! The awaited `on_event` hook: ordering before listeners, `message_end` replacement, and
//! tool updates delivered in order before `tool_execution_end`.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use pi_agent::{Agent, AgentEvent, AgentTool, AgentToolResult, ToolUpdateCallback};
use pi_ai::{ContentBlock, Message};
use serde_json::{Value, json};
use support::*;
use tokio_util::sync::CancellationToken;

struct Streaming;

#[async_trait]
impl AgentTool for Streaming {
    fn name(&self) -> &str {
        "stream"
    }

    fn description(&self) -> &str {
        "sends three updates"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(
        &self,
        _id: &str,
        _args: Value,
        _cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        for step in 0..3 {
            on_update(AgentToolResult::text(format!("step {step}")));
        }
        Ok(AgentToolResult::text("done"))
    }
}

fn label(event: &AgentEvent) -> String {
    match event {
        AgentEvent::ToolExecutionUpdate { partial_result, .. } => {
            format!("update:{}", partial_result.content[0].as_text().unwrap_or_default())
        }
        other => kind(other),
    }
}

#[tokio::test]
async fn hook_runs_before_listeners_replaces_messages_and_orders_updates() {
    let (_, stream_fn) = sequence(vec![tool_use(vec![call("c1", "stream", json!({}))]), text("original")]);
    let agent = Agent::new(model());
    agent.update_state(|state| state.tools = vec![Arc::new(Streaming)]);
    let order = Arc::new(Mutex::new(Vec::<String>::new()));
    let hook_order = order.clone();
    agent.update_hooks(|hooks| {
        hooks.stream_fn = Some(stream_fn);
        hooks.on_event = Some(Arc::new(move |event, _cancel| {
            let order = hook_order.clone();
            async move {
                // A slow handler must still finish before listeners and the next event.
                tokio::time::sleep(Duration::from_millis(1)).await;
                order.lock().unwrap().push(format!("hook:{}", label(&event)));
                match event {
                    AgentEvent::MessageEnd { message: Message::Assistant(mut assistant) }
                        if assistant.text() == "original" =>
                    {
                        assistant.content = vec![ContentBlock::text("replaced")];
                        Some(Message::Assistant(assistant))
                    }
                    _ => None,
                }
            }
            .boxed()
        }));
    });
    let listener_order = order.clone();
    agent.subscribe(move |event| {
        let text = match event {
            AgentEvent::MessageEnd { message: Message::Assistant(assistant) } => format!(":{}", assistant.text()),
            _ => String::new(),
        };
        listener_order.lock().unwrap().push(format!("listener:{}{text}", label(event)));
    });
    agent.prompt("go", vec![]).await.unwrap();

    let order = order.lock().unwrap().clone();
    let updates: Vec<&String> = order.iter().filter(|entry| entry.starts_with("hook:update")).collect();
    assert_eq!(updates, ["hook:update:step 0", "hook:update:step 1", "hook:update:step 2"]);
    let last_update = order.iter().position(|entry| entry == "listener:update:step 2").unwrap();
    let end = order.iter().position(|entry| entry == "hook:tool_execution_end").unwrap();
    assert!(last_update < end, "updates precede the end: {order:?}");
    for pair in order.chunks(2) {
        let hook = pair[0].strip_prefix("hook:").expect("hook first");
        let listener = pair[1].strip_prefix("listener:").expect("listener second");
        assert!(listener.starts_with(hook), "{order:?}");
    }
    assert!(order.contains(&"listener:message_end:replaced".to_string()), "{order:?}");
    let last = agent.state().messages.last().cloned().unwrap();
    assert!(matches!(last, Message::Assistant(assistant) if assistant.text() == "replaced"));
}
