//! Guards of the account model lists: no redirects, unrelated lists, and lists without the
//! provider's default model.

use serde_json::json;

use super::reconcile::{AddRule, Listed, reconcile};
use super::tests_accounts::{Saved, chat, context, ids};
use super::tests_openrouter::serve;
use super::*;
use crate::providers::http_mock::{Reply, RouteServer};

fn claude_catalog() -> Vec<Model> {
    let claude = |id| chat("anthropic", "anthropic-messages", id);
    vec![claude("claude-opus-4-8"), claude("claude-sonnet-5"), claude("claude-haiku-4-5")]
}

#[tokio::test]
async fn list_requests_do_not_follow_redirects() {
    for (list, path) in [(AccountModelList::Anthropic, "/v1/models"), (AccountModelList::Google, "/models")] {
        let elsewhere = serve(200, json!({"data": [], "models": []})).await;
        let target = format!("{}{path}", elsewhere.base_url);
        let server = RouteServer::start(move |_| Reply::new(302, "text/plain", "").header("location", &target)).await;
        // The production client, as pi sends it.
        let context =
            ModelListContext { base_url: Some(server.base_url.clone()), ..ModelListContext::new(Some("key".into())) };
        let error = list.personalize(claude_catalog(), &context).await.unwrap_err();
        assert!(error.contains("302"), "{list:?}: {error}");
        assert!(elsewhere.requests().is_empty(), "{list:?} sends the key to the configured host only");
        assert_eq!(server.requests().len(), 1);
    }
}

#[tokio::test]
async fn a_list_without_catalog_models_changes_and_saves_nothing() {
    let unrelated = json!({"data": [{"id": "llama-proxy-1"}, {"id": "mistral-proxy"}], "has_more": false});
    for body in [unrelated, json!({"data": []})] {
        let server = serve(200, body).await;
        let list = AccountModelList::Anthropic;
        let result = list.personalize(claude_catalog(), &context(&server, Some("k"))).await.unwrap();
        assert_eq!(result, Personalized::Unmatched(claude_catalog()));
    }
}

#[test]
fn a_list_without_the_default_model_only_enriches_the_catalog() {
    let listed =
        [Listed { context_window: Some(300_000), ..Listed::new("claude-sonnet-5") }, Listed::new("claude-x-1")];
    let models = reconcile(claude_catalog(), &listed, AddRule::Any, Some("claude-opus-4-8")).saved();
    assert_eq!(ids(&models), ["claude-opus-4-8", "claude-sonnet-5", "claude-haiku-4-5", "claude-x-1"]);
    assert_eq!(models[1].context_window, 300_000);

    let listed = [Listed::new("claude-opus-4-8")];
    let models = reconcile(claude_catalog(), &listed, AddRule::Any, Some("claude-opus-4-8")).saved();
    assert_eq!(ids(&models), ["claude-opus-4-8"], "a list with the default narrows the catalog");
    let listed = [Listed::new("claude-sonnet-5")];
    let models = reconcile(claude_catalog(), &listed, AddRule::Any, Some("claude-opus-9")).saved();
    assert_eq!(ids(&models), ["claude-sonnet-5"], "a default the catalog lacks does not count");
}

#[tokio::test]
async fn the_default_model_comes_from_the_context() {
    let server = serve(200, json!({"data": [{"id": "claude-haiku-4-5"}]})).await;
    let context = ModelListContext { default_model: Some("claude-opus-4-8".into()), ..context(&server, Some("k")) };
    let models = AccountModelList::Anthropic.personalize(claude_catalog(), &context).await.saved();
    assert_eq!(ids(&models), ["claude-opus-4-8", "claude-sonnet-5", "claude-haiku-4-5"]);
}
