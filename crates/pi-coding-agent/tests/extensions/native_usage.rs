//! `usage.get` for an extension provider that logs in with `oauth: { builtin }`: pi reads the
//! usage endpoint of the built-in provider with the slot's own login.

use std::sync::Arc;

use pi_ai::auth::{Credential, CredentialStore, OAuthAuth, OAuthCredential};
use pi_coding_agent::extension_ui::ExtensionMode;
use serde_json::{Value, json};

use crate::harness::Setup;
use crate::native::{NativeUi, entry_data, native_in};
use crate::native_contracts::FakeAnthropic;

#[tokio::test(flavor = "multi_thread")]
async fn usage_get_reads_a_builtin_oauth_slot_with_its_own_login() {
    let body = json!({"plan_type": "max", "five_hour": {"utilization": 42, "resets_at": "2026-10-30T17:00:00Z"},
        "seven_day_opus": {"utilization": 7, "resets_at": null}});
    let server = crate::http_mock::Server::start(move |_| crate::http_mock::Reply {
        status: 200,
        content_type: "application/json",
        body: body.to_string().into_bytes(),
    })
    .await;
    let flow = Arc::new(FakeAnthropic::default());
    let oauth: pi_ai::auth::OAuthLookup =
        Arc::new(move |id: &str| (id == "anthropic").then(|| flow.clone() as Arc<dyn OAuthAuth>));
    let h = native_in(
        Setup { oauth: Some(oauth), ..Setup::default() },
        &[],
        ExtensionMode::Tui,
        Arc::new(NativeUi::default()),
        false,
    )
    .await;
    let url = server.url.clone();
    let endpoints =
        pi_ai::usage::UsageEndpoints { anthropic: url.clone(), chatgpt: url.clone(), cursor: url.clone(), grok: url };
    h.session.model_runtime().usage().set_endpoints(endpoints);
    let credential = OAuthCredential {
        refresh: "r".into(),
        access: "slot-token".into(),
        expires: pi_ai::now_ms() + 3_600_000,
        extra: Default::default(),
    };
    let stored = Credential::OAuth(credential);
    h.credentials.update("anthropic-2", Box::new(move |_| Box::pin(async move { Ok(Some(stored)) }))).await.unwrap();
    h.session.prompt("/native-oauth", Vec::new()).await.unwrap();
    h.session.prompt("/native-usage-slot", Vec::new()).await.unwrap();
    let usage = entry_data(&h, "native-usage-slot").unwrap();
    let slot = &usage["providers"][0];
    assert_eq!(
        (&slot["provider"], &slot["plan"], &slot["error"]),
        (&json!("anthropic-2"), &json!("max"), &Value::Null),
        "{usage}"
    );
    assert_eq!(
        (&slot["windows"][1]["scope"], &slot["windows"][1]["families"]),
        (&json!("model"), &json!(["opus"])),
        "{usage}"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        (requests[0].path(), requests[0].header("authorization")),
        ("/api/oauth/usage", Some("Bearer slot-token"))
    );
}
