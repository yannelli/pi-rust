//! Native protocol contracts beyond the first release of version 1: `tool.prepare`, renderers,
//! `settings.set`, `oauth: { builtin }`, crash unregistering, and the order of provider events.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use pi_ai::auth::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthResult, AuthType, Credential, CredentialStore, LoginOptions, ModelAuth,
    OAuthAuth, OAuthCredential,
};
use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::extension_ui::ExtensionMode;
use pi_coding_agent::extensions::ToolRenderRequest;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::harness::{Harness, Setup, eventually};
use crate::native::{NativeUi, entry_data, native, native_in, render_until};

fn tool_names(h: &Harness) -> (Vec<String>, Vec<String>) {
    let all = h.session.all_tools().into_iter().map(|tool| tool.name).collect();
    (all, h.session.active_tool_names())
}

#[tokio::test(flavor = "multi_thread")]
async fn prepare_arguments_run_before_validation() {
    let (h, _ui) =
        native(vec![FauxResponse::tool_call("c1", "native_draw", json!({"text": 5})), FauxResponse::text("done")])
            .await;
    h.session.prompt("draw five", Vec::new()).await.unwrap();
    let transcript = h.transcript();
    assert!(transcript.contains(&"toolResult: drew coerced 5".to_string()), "{transcript:?}");
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[tokio::test(flavor = "multi_thread")]
async fn renderers_draw_child_lines_for_each_width_from_a_cache() {
    let (h, _ui) = native(Vec::new()).await;
    let info = h.runner.tool_renderer_info("native_draw").expect("native_draw has renderers");
    assert!(info.call && info.result && !info.self_shell, "{info:?}");
    let request = ToolRenderRequest {
        tool_call_id: "r1".into(),
        args: json!({"text": "hi"}),
        args_complete: true,
        execution_started: true,
        ..ToolRenderRequest::default()
    };
    let result = json!({"content": [{"type": "text", "text": "drew hi"}], "details": null, "isError": false});
    let message = json!({"role": "custom", "customType": "native-card", "content": "teal card", "display": true});
    let entry = json!({"type": "custom", "id": "e1", "customType": "native-note", "data": {"color": "plum"}});
    let runner = h.runner.clone();
    let lines = tokio::task::block_in_place(move || {
        let mut call = runner.render_tool_call("native_draw", &request).expect("a call component");
        let wide = render_until(&mut call, 40, "CALL hi at 40");
        let narrow = render_until(&mut call, 25, "CALL hi at 25");
        let mut done = runner.render_tool_result("native_draw", &result, &request).expect("a result component");
        let drawn = render_until(&mut done, 30, "RESULT drew hi at 30");
        let mut card = runner.render_message("native-card", &message, false, 0).expect("a message component");
        let card_lines = render_until(&mut card, 30, "CARD teal card at 30");
        let mut rebuilt = runner.render_message("native-card", &message, false, 0).expect("a message component");
        let cached = pi_tui::Component::render(&mut rebuilt, 30);
        let mut note = runner.render_entry(&entry, true).unwrap().expect("an entry component");
        let note_lines = render_until(&mut note, 30, "NOTE plum at 30 open");
        [wide, narrow, drawn, card_lines, cached, note_lines]
    });
    let expected =
        ["CALL hi at 40", "CALL hi at 25", "RESULT drew hi at 30", "CARD teal card at 30", "CARD teal card at 30"];
    for (lines, text) in lines.iter().zip(expected) {
        assert_eq!(lines, &vec![text.to_string()]);
    }
    assert_eq!(lines[5], vec!["NOTE plum at 30 open".to_string()]);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_set_writes_one_namespace_and_keeps_other_keys() {
    let (h, _ui) = native(Vec::new()).await;
    h.session.prompt("/native-settings", Vec::new()).await.unwrap();
    let data = entry_data(&h, "native-settings").unwrap();
    assert_eq!(data["global"], json!({"result": null}), "{data}");
    assert_eq!(data["read"], json!({"color": "teal"}), "{data}");
    assert_eq!(data["project"], json!({"result": null}), "{data}");
    assert_eq!(data["merged"], json!({"color": "plum"}), "{data}");
    assert_eq!(data["piKey"]["code"], json!(-32602), "{data}");
    assert_eq!(data["badScope"]["code"], json!(-32602), "{data}");
    let settings = &h.services.settings;
    let global = settings.global_settings().extra;
    assert_eq!(global.get("nativeFixture"), Some(&json!({"color": "teal"})));
    assert_eq!(settings.global_settings().default_model.as_deref(), Some("tiny"));
    assert_eq!(settings.project_settings().extra.get("nativeFixture"), Some(&json!({"color": "plum"})));
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_set_refuses_the_project_scope_of_an_untrusted_project() {
    let (h, _ui) = native(Vec::new()).await;
    h.services.settings.set_project_trusted(false);
    h.session.prompt("/native-settings", Vec::new()).await.unwrap();
    let data = entry_data(&h, "native-settings").unwrap();
    assert_eq!(data["global"], json!({"result": null}), "{data}");
    assert_eq!(data["project"]["code"], json!(-32003), "{data}");
    assert_eq!(data["merged"], json!({"color": "teal"}), "{data}");
    assert_eq!(h.services.settings.project_settings().extra.get("nativeFixture"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn crashed_child_tools_leave_the_registry_and_return_after_a_reload() {
    let (h, _ui) = native(Vec::new()).await;
    let has = |names: &[String], name: &str| names.iter().any(|candidate| candidate == name);
    let (all, active) = tool_names(&h);
    assert!(has(&all, "native_echo") && has(&active, "native_echo"), "{all:?} {active:?}");
    h.session.prompt("/native-crash", Vec::new()).await.unwrap();
    eventually(|| {
        let (all, active) = tool_names(&h);
        !has(&all, "native_echo") && !has(&all, "native_draw") && !has(&active, "native_echo")
    })
    .await;
    h.session.reload_extensions().await.unwrap();
    eventually(|| {
        let (all, active) = tool_names(&h);
        has(&all, "native_echo") && has(&active, "native_echo") && has(&active, "native_draw")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn after_provider_response_finishes_before_message_end() {
    let (h, _ui) = native(vec![FauxResponse::text("ordered")]).await;
    h.session.prompt("/native-order", Vec::new()).await.unwrap();
    h.session.prompt("go", Vec::new()).await.unwrap();
    eventually(|| entry_data(&h, "native-order").is_some()).await;
    let data = entry_data(&h, "native-order").unwrap();
    assert_eq!(data["order"], json!(["after_provider_response 200", "message_end"]), "{data}");
}

/// The core `anthropic` flow of the test: counts logins and refreshes and makes no request.
#[derive(Default)]
pub(crate) struct FakeAnthropic {
    logins: AtomicUsize,
    refreshes: AtomicUsize,
}

#[async_trait]
impl OAuthAuth for FakeAnthropic {
    fn name(&self) -> &str {
        "Anthropic (Claude Pro/Max)"
    }

    async fn login(
        &self,
        _: &dyn AuthInteraction,
        _: &LoginOptions,
        _: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        self.logins.fetch_add(1, Ordering::SeqCst);
        Ok(OAuthCredential {
            refresh: "r1".into(),
            access: "login-access".into(),
            expires: 1,
            extra: Default::default(),
        })
    }

    async fn refresh(&self, credential: &OAuthCredential, _: CancellationToken) -> AuthResult<OAuthCredential> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        assert_eq!(credential.refresh, "r1");
        let expires = pi_ai::now_ms() + 3_600_000;
        Ok(OAuthCredential {
            refresh: "r2".into(),
            access: "refreshed-access".into(),
            expires,
            extra: Default::default(),
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}

struct NoInteraction;

#[async_trait]
impl AuthInteraction for NoInteraction {
    async fn prompt(&self, _: AuthPrompt, _: CancellationToken) -> AuthResult<String> {
        Ok(String::new())
    }

    fn notify(&self, _: AuthEvent) {}
}

async fn stored_access(h: &Harness, provider: &str) -> Option<String> {
    match h.credentials.read(provider).await.unwrap() {
        Some(Credential::OAuth(credential)) => Some(credential.access),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_builtin_reuses_the_core_login_and_refresh() {
    let fake = Arc::new(FakeAnthropic::default());
    let flow = fake.clone();
    let oauth: pi_ai::auth::OAuthLookup =
        Arc::new(move |id: &str| (id == "anthropic").then(|| flow.clone() as Arc<dyn OAuthAuth>));
    let setup = Setup { oauth: Some(oauth), ..Setup::default() };
    let ui = Arc::new(NativeUi::default());
    let h = native_in(setup, &[], ExtensionMode::Tui, ui.clone(), false).await;
    h.session.prompt("/native-oauth", Vec::new()).await.unwrap();
    let data = entry_data(&h, "native-oauth").unwrap();
    assert_eq!(data["registered"], json!({"result": null}), "{data}");
    assert_eq!(data["unknown"], json!({"result": null}), "{data}");
    let unknown = h.errors().into_iter().find(|error| error.starts_with("register_provider"));
    assert!(unknown.is_some_and(|error| error.contains("no built-in OAuth login \"nope\"")), "{:?}", h.errors());

    h.session.prompt("/native-login", Vec::new()).await.unwrap();
    let login = entry_data(&h, "native-login").unwrap();
    assert_eq!(login["slot"], json!({"result": true}), "{login}");
    assert_eq!(login["foreign"]["code"], json!(-32602), "{login}");
    assert!(ui.calls().contains(&"login: anthropic-2".to_string()), "{:?}", ui.calls());

    let runtime = h.session.model_runtime().clone();
    let options = runtime.login_providers(Some(AuthType::OAuth)).await;
    let slot = options.iter().find(|option| option.id == "anthropic-2").expect("anthropic-2 is a login option");
    assert_eq!((slot.name.as_str(), slot.login_label.as_deref()), ("Anthropic #2", Some("Anthropic #2")));
    let cancel = CancellationToken::new();
    runtime.login("anthropic-2", AuthType::OAuth, &NoInteraction, &LoginOptions::default(), cancel).await.unwrap();
    assert_eq!(fake.logins.load(Ordering::SeqCst), 1);
    assert_eq!(stored_access(&h, "anthropic-2").await.as_deref(), Some("login-access"));
    assert_eq!(stored_access(&h, "anthropic").await, None);

    h.session.prompt("/native-auth", Vec::new()).await.unwrap();
    let auth = entry_data(&h, "native-auth").unwrap();
    assert_eq!(auth["result"]["apiKey"], json!("refreshed-access"), "{auth}");
    assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(stored_access(&h, "anthropic-2").await.as_deref(), Some("refreshed-access"));
}

async fn message_renders(h: &Harness) -> u64 {
    h.session.prompt("/native-render-count", Vec::new()).await.unwrap();
    let counts: Vec<u64> = h
        .entries()
        .iter()
        .filter(|entry| entry["customType"] == "native-render-count")
        .map(|entry| entry["data"]["count"].as_u64().unwrap())
        .collect();
    *counts.last().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalidated_renderer_asks_the_child_again_once() {
    let (h, _ui) = native(Vec::new()).await;
    let message = json!({"role": "custom", "customType": "native-card", "content": "teal card", "display": true});
    let runner = h.runner.clone();
    let mut card = tokio::task::block_in_place(|| runner.render_message("native-card", &message, false, 0)).unwrap();
    let first = tokio::task::block_in_place(|| render_until(&mut card, 30, "CARD teal card at 30"));
    assert_eq!(first, vec!["CARD teal card at 30".to_string()]);
    h.session.prompt("/native-restyle", Vec::new()).await.unwrap();
    let cached = tokio::task::block_in_place(|| pi_tui::Component::render(&mut card, 30));
    assert_eq!(cached, first, "a known width sends no request");
    let fresh = tokio::task::block_in_place(|| {
        pi_tui::Component::invalidate(&mut card);
        render_until(&mut card, 30, "CARD teal card at 30 bold")
    });
    assert_eq!(fresh, vec!["CARD teal card at 30 bold".to_string()]);
    let settled = message_renders(&h).await;
    tokio::task::block_in_place(|| {
        for _ in 0..10 {
            pi_tui::Component::render(&mut card, 30);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    assert_eq!(message_renders(&h).await, settled, "redraws after the answer sent requests");
    assert_eq!(settled, 2);
}
