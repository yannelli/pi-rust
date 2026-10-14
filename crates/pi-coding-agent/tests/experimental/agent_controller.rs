//! Port of upstream `test/experimental-agent-controller.test.ts`.

use pi_chord::facets::{FacetOptions, create_facet_host, define_facet};
use pi_chord::services::{ServiceCall, ServiceMode};
use pi_chord::{BACKGROUND_CONTEXT, Json};
use pi_coding_agent::experimental::services::agent_controller::{
    AGENT_CONTROLLER, AgentCompactionRequest, AgentPromptRequest, agent_controller_impl, create_agent_controller,
};
use pi_durable::testing::faux::{FauxMessageOptions, faux_assistant_message};
use serde_json::json;

use super::durable_support::{open_faux_conversation, pending_response};

fn call(member: &str, args: Vec<serde_json::Value>) -> ServiceCall {
    let args = args.iter().map(Json::from).collect();
    ServiceCall { service_id: AGENT_CONTROLLER.id().into(), instance: None, member: member.into(), args }
}

#[tokio::test]
async fn prompts_the_root_conversation_through_the_service_catalogue() {
    let answer = faux_assistant_message("hello back", FauxMessageOptions::default());
    let faux = open_faux_conversation(vec![answer.into()]).await;
    let controller = create_agent_controller(faux.harness.clone(), faux.conversation.clone());
    let facet = define_facet("test-agent-controller", move |env| {
        env.provide(&AGENT_CONTROLLER, agent_controller_impl(controller.clone()))
    });
    let host = create_facet_host(FacetOptions::new(vec![facet])).await.unwrap();
    let catalogue = host.services().catalogue();
    assert_eq!(catalogue.len(), 1);
    assert_eq!((catalogue[0].service_id.as_str(), catalogue[0].mode), (AGENT_CONTROLLER.id(), ServiceMode::Singleton));
    let request = json!({ "message": "hello", "images": null });
    let response =
        host.services().invoke(call("prompt", vec![request]), &BACKGROUND_CONTEXT).await.unwrap().unwrap().to_serde();
    assert_eq!(response["accepted"], true);
    assert_eq!(response["error"], serde_json::Value::Null);
    let operation_id = response["operationId"].as_str().unwrap().to_string();
    let settled = host.services().invoke(call("waitForPrompt", vec![json!(operation_id)]), &BACKGROUND_CONTEXT).await;
    assert_eq!(settled.unwrap().unwrap().to_serde(), json!({ "status": "done", "text": "hello back", "reason": null }));
    host.dispose().await.unwrap();
    faux.close().await;
}

#[tokio::test]
async fn rejects_a_prompt_while_busy_and_queues_steering_and_follow_up_input() {
    let (step, reached) = pending_response();
    let faux = open_faux_conversation(vec![step]).await;
    let controller = create_agent_controller(faux.harness.clone(), faux.conversation.clone());
    let context = &BACKGROUND_CONTEXT;
    let first = controller.prompt(AgentPromptRequest::text("first"), context).await.unwrap();
    assert!(first.accepted);
    reached.notified().await;

    let second = controller.prompt(AgentPromptRequest::text("second"), context).await.unwrap();
    assert!(!second.accepted && second.operation_id.is_none());
    let error = second.error.unwrap();
    assert_eq!(error.code, "busy");
    assert!(error.message.contains("busy"));
    let steer = controller.steer(AgentPromptRequest::text("steer"), context).await.unwrap();
    let follow_up = controller.follow_up(AgentPromptRequest::text("later"), context).await.unwrap();
    assert!(steer.accepted && steer.entry_id.is_some() && steer.error.is_none());
    assert!(follow_up.accepted && follow_up.entry_id.is_some() && follow_up.error.is_none());
    let view = faux.conversation.view_state(context).await.unwrap();
    let modes: Vec<String> = view.value().to_serde()["docs"]["pi.inbox"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["mode"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(modes, ["steer", "followUp"]);
    view.dispose().unwrap();

    let entry_id = follow_up.entry_id.unwrap();
    assert_eq!(controller.cancel_queued(entry_id.clone(), context).await.unwrap().outcome, "cancelled");
    assert_eq!(controller.cancel_queued(entry_id, context).await.unwrap().outcome, "already_consumed");
    assert_eq!(controller.cancel_queued("999".into(), context).await.unwrap().outcome, "not_found");
    assert_eq!(controller.cancel_queued("not-an-id".into(), context).await.unwrap().outcome, "not_found");

    controller.abort(context).await.unwrap();
    let view = faux.conversation.view_state(context).await.unwrap();
    assert!(view.value().to_serde()["docs"]["pi.live"].get("run").is_none());
    view.dispose().unwrap();
    let settled = controller.wait_for_prompt(first.operation_id.unwrap(), context).await.unwrap();
    assert_eq!((settled.status.as_str(), settled.text), ("unanswered", None));
    faux.close().await;
}

#[tokio::test]
async fn starts_a_compaction_task() {
    let faux = open_faux_conversation(Vec::new()).await;
    let controller = create_agent_controller(faux.harness.clone(), faux.conversation.clone());
    let request = AgentCompactionRequest { custom_instructions: Some("short".into()) };
    let response = controller.compact(request, &BACKGROUND_CONTEXT).await.unwrap();
    assert!(response.accepted && response.operation_id.is_some() && response.error.is_none());
    faux.close().await;
}
