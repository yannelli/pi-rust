//! Login dialog, auth URL copy, and login flows through the dialog; ports `auth-url-copy.test.ts`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use pi_ai::auth::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthResult, AuthType, Credential, CredentialStore, InMemoryCredentialStore,
    LoginOptions, ModelAuth, OAuthAuth, OAuthCredential, PromptOption,
};
use tokio_util::sync::CancellationToken;

use crate::agent_session::{ModelRuntime, ModelRuntimeOptions};
use crate::interactive_components::login::{ClipboardCopyFn, DeviceCodeInfo, LoginDialog, run_dialog_login};
use crate::interactive_components::test_support::{Calls, Harness, flush, keys, local};

const URL: &str = "https://auth.example.invalid/authorize?xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";

pub(super) fn dialog_harness(
    copied: &Calls<String>,
    opened: &Calls<String>,
    completed: &Calls<(bool, Option<String>)>,
) -> Harness<LoginDialog> {
    let (copied, opened, completed) = (copied.clone(), opened.clone(), completed.clone());
    Harness::new(100, 30, move |tui| {
        let mut dialog = LoginDialog::new(
            tui,
            "test",
            move |success, message: Option<&str>| completed.push((success, message.map(str::to_string))),
            Some("Test Provider"),
            None,
        );
        let copier: ClipboardCopyFn = Rc::new(move |text: String| {
            copied.push(text);
            Box::pin(async { Ok(()) })
        });
        dialog.set_clipboard(copier);
        dialog.set_browser_opener(Rc::new(move |url: &str| opened.push(url.to_string())));
        dialog
    })
}

#[tokio::test(flavor = "current_thread")]
async fn login_dialog_copies_the_auth_url_instead_of_typing_into_the_code_input() {
    local(async {
        let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
        let harness = dialog_harness(&copied, &opened, &completed);
        let _pending = {
            let mut dialog = harness.component.borrow_mut();
            dialog.show_auth(URL, Some("Complete the sign-in in your browser."));
            dialog.show_manual_input("Paste the code:")
        };
        harness.settle();
        let screen = harness.screen();
        assert!(screen.contains(" Login to Test Provider"), "{screen}");
        assert!(screen.contains("ctrl+x to copy"), "{screen}");
        assert!(screen.contains("Complete the sign-in in your browser."));
        assert!(screen.contains("Paste the code:"));
        assert_eq!(opened.get(), [URL]);
        harness.send(&keys::ctrl('x'));
        flush(&harness).await;
        assert!(harness.screen().contains("Copied URL to clipboard"));
        assert_eq!(copied.get(), [URL]);
    })
    .await;
}

#[test]
fn login_dialog_ignores_the_copy_key_without_an_auth_url() {
    let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
    let harness = dialog_harness(&copied, &opened, &completed);
    let device = DeviceCodeInfo {
        user_code: "ABCD".into(),
        verification_uri: "https://example.invalid/device".into(),
        ..DeviceCodeInfo::default()
    };
    harness.component.borrow_mut().show_device_code(&device);
    harness.component.borrow_mut().show_waiting("Waiting for authentication...");
    harness.send(&keys::ctrl('x'));
    let screen = harness.screen();
    assert!(screen.contains("https://example.invalid/device"), "{screen}");
    assert!(screen.contains("Ctrl+click to open") || screen.contains("Cmd+click to open"));
    assert!(screen.contains("Enter code: ABCD"));
    assert!(screen.contains("Waiting for authentication..."));
    assert!(copied.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn prompts_resolve_with_submitted_text_and_cancel_rejects() {
    local(async {
        let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
        let harness = dialog_harness(&copied, &opened, &completed);
        let answer = harness.component.borrow_mut().show_prompt("Enter workspace", Some("acme"));
        harness.settle();
        let screen = harness.screen();
        assert!(screen.contains("Enter workspace") && screen.contains("e.g., acme"), "{screen}");
        assert!(screen.contains("(escape/ctrl+c to cancel, enter to submit)"));
        harness.type_text("acme");
        harness.send(keys::ENTER);
        assert_eq!(answer.await.unwrap(), "acme");
        assert!(harness.screen().contains("> acme"));

        let pending = harness.component.borrow_mut().show_manual_input("Paste");
        let token = harness.component.borrow().cancel_token();
        harness.send(keys::ESCAPE);
        assert!(pending.await.is_err());
        assert!(token.is_cancelled());
        assert_eq!(completed.get(), [(false, Some("Login cancelled".to_string()))]);
    })
    .await;
}

#[test]
fn details_and_info_render_links_and_close_hint() {
    let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
    let harness = dialog_harness(&copied, &opened, &completed);
    {
        let mut dialog = harness.component.borrow_mut();
        dialog.show_details(&["You can also use an AWS profile.".to_string()]);
        let links = [pi_ai::auth::AuthInfoLink { url: "https://docs.example".into(), label: Some("Docs".into()) }];
        dialog.show_info("Authentication is configured outside pi.", &links, true);
        dialog.show_progress("Checking...");
    }
    harness.settle();
    let screen = harness.screen();
    assert!(screen.contains("You can also use an AWS profile."), "{screen}");
    assert!(screen.contains("Authentication is configured outside pi."));
    assert!(screen.contains("Docs: https://docs.example"));
    assert!(screen.contains("(escape/ctrl+c to close)"));
    assert!(screen.contains("Checking..."));
}

struct FakeOAuth;

#[async_trait]
impl OAuthAuth for FakeOAuth {
    fn name(&self) -> &str {
        "Fake"
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let options = vec![
            PromptOption { id: "eu".into(), label: "Europe".into(), description: None },
            PromptOption { id: "us".into(), label: "United States".into(), description: None },
        ];
        let region =
            interaction.prompt(AuthPrompt::Select { message: "Region".into(), options }, cancel.clone()).await?;
        interaction.notify(AuthEvent::AuthUrl { url: format!("https://login.example/{region}"), instructions: None });
        let prompt = AuthPrompt::ManualCode { message: "Paste the redirect URL".into(), placeholder: None };
        let code = interaction.prompt(prompt, cancel).await?;
        interaction.notify(AuthEvent::Progress { message: "Exchanging code...".into() });
        Ok(OAuthCredential {
            refresh: format!("{region}:{code}"),
            access: "token".into(),
            expires: 1,
            ..Default::default()
        })
    }

    async fn refresh(&self, credential: &OAuthCredential, _cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        Ok(credential.clone())
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..Default::default() }
    }
}

async fn runtime(store: Arc<InMemoryCredentialStore>) -> Arc<ModelRuntime> {
    let dir = std::env::temp_dir().join("pi-login-dialog-test-missing");
    let oauth: pi_ai::auth::OAuthLookup =
        Arc::new(|id: &str| (id == "fake").then(|| Arc::new(FakeOAuth) as Arc<dyn OAuthAuth>));
    let options = ModelRuntimeOptions {
        models_path: dir.join("models.json"),
        credentials: store,
        oauth: Some(oauth),
        credential_env: Arc::new(|_: &str| None::<String>),
    };
    Arc::new(ModelRuntime::create(options).await)
}

#[tokio::test(flavor = "current_thread")]
async fn oauth_login_runs_through_the_dialog_and_stores_the_credential() {
    local(async {
        let store = Arc::new(InMemoryCredentialStore::new(BTreeMap::new()));
        let runtime = runtime(store.clone()).await;
        let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
        let harness = dialog_harness(&copied, &opened, &completed);
        let dialog = harness.component.clone();
        let result: Rc<RefCell<Option<AuthResult<Credential>>>> = Rc::default();
        let slot = result.clone();
        let task_runtime = runtime.clone();
        tokio::task::spawn_local(async move {
            let options = LoginOptions::default();
            let outcome = run_dialog_login(&dialog, &task_runtime, "fake", AuthType::OAuth, &options).await;
            *slot.borrow_mut() = Some(outcome);
        });
        flush(&harness).await;
        let screen = harness.screen();
        assert!(screen.contains("Region") && screen.contains("→ Europe"), "{screen}");
        harness.send(keys::DOWN);
        harness.send(keys::ENTER);
        flush(&harness).await;
        let screen = harness.screen();
        assert!(screen.contains("https://login.example/us"), "{screen}");
        assert!(screen.contains("Paste the redirect URL"));
        assert_eq!(opened.get(), ["https://login.example/us"]);
        harness.type_text("abc");
        harness.send(keys::ENTER);
        flush(&harness).await;
        assert!(harness.screen().contains("Exchanging code..."));
        let outcome = result.borrow_mut().take().expect("login finished");
        let Credential::OAuth(credential) = outcome.unwrap() else { panic!("expected OAuth") };
        assert_eq!(credential.refresh, "us:abc");
        assert!(matches!(store.read("fake").await.unwrap(), Some(Credential::OAuth(_))));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn api_key_login_prompts_for_the_key_and_cancel_aborts() {
    local(async {
        let store = Arc::new(InMemoryCredentialStore::new(BTreeMap::new()));
        let runtime = runtime(store.clone()).await;
        let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
        let harness = dialog_harness(&copied, &opened, &completed);
        let result: Rc<RefCell<Option<AuthResult<Credential>>>> = Rc::default();
        let (dialog, slot, task_runtime) = (harness.component.clone(), result.clone(), runtime.clone());
        tokio::task::spawn_local(async move {
            let options = LoginOptions::default();
            let outcome = run_dialog_login(&dialog, &task_runtime, "openai", AuthType::ApiKey, &options).await;
            *slot.borrow_mut() = Some(outcome);
        });
        flush(&harness).await;
        assert!(harness.screen().contains("API key"), "{}", harness.screen());
        harness.type_text("sk-test");
        harness.send(keys::ENTER);
        flush(&harness).await;
        assert!(result.borrow_mut().take().expect("finished").is_ok());
        let Some(Credential::ApiKey(stored)) = store.read("openai").await.unwrap() else { panic!("stored key") };
        assert_eq!(stored.key.as_deref(), Some("sk-test"));

        let harness = dialog_harness(&copied, &opened, &completed);
        let (dialog, slot) = (harness.component.clone(), result.clone());
        tokio::task::spawn_local(async move {
            let options = LoginOptions::default();
            let outcome = run_dialog_login(&dialog, &runtime, "fake", AuthType::OAuth, &options).await;
            *slot.borrow_mut() = Some(outcome);
        });
        flush(&harness).await;
        harness.send(keys::ESCAPE);
        flush(&harness).await;
        assert!(matches!(result.borrow_mut().take(), Some(Err(pi_ai::auth::AuthError::Cancelled))));
    })
    .await;
}
