//! Session replacement through the runtime (upstream `agent-session-runtime` and
//! `agent-session-branching` tests).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::auth::{
    AuthError, AuthInteraction, AuthOverrides, AuthResult, Credential, CredentialStore, FileCredentialStore,
    LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};
use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::agent_session::{AgentSessionRuntime, ForkPosition};
use pi_coding_agent::app::AppOptions;
use pi_coding_agent::session::SessionManager;
use serde_json::json;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::agent_session::harness::{Harness, Options, harness, roles, text};

async fn runtime_with_turns(persist: bool) -> (Harness, AgentSessionRuntime) {
    let responses =
        vec![FauxResponse::text("first answer"), FauxResponse::text("second answer"), FauxResponse::text("third")];
    let h = harness(Options { responses, persist, ..Options::default() }).await;
    let manager = if persist {
        SessionManager::create(&h.cwd, Some(&h.agent_dir.join("sessions")))
    } else {
        SessionManager::in_memory(&h.cwd)
    };
    let runtime = crate::offline_guard::app_runtime(h.app_options(), manager).await;
    let session = runtime.session();
    session.prompt("first question", Vec::new()).await.unwrap();
    session.prompt("second question", Vec::new()).await.unwrap();
    (h, runtime)
}

fn user_ids(runtime: &AgentSessionRuntime) -> Vec<String> {
    runtime.session().user_messages_for_forking().into_iter().map(|(id, _)| id).collect()
}

#[tokio::test]
async fn starts_new_sessions_and_switches_back_with_rebinding() {
    let (_h, runtime) = runtime_with_turns(true).await;
    let first = runtime.session();
    let first_file = first.session_file().unwrap();
    let rebound = Arc::new(Mutex::new(Vec::new()));
    let sink = rebound.clone();
    runtime.set_rebind_session(Some(Arc::new(move |session| sink.lock().unwrap().push(session.session_id()))));

    runtime.new_session(None, None).await.unwrap();
    let fresh = runtime.session();
    assert_ne!(fresh.session_id(), first.session_id());
    assert!(fresh.messages().is_empty());
    assert_eq!(fresh.model().unwrap().id, "tiny");

    runtime.switch_session(&first_file, None).await.unwrap();
    let resumed = runtime.session();
    assert_eq!(resumed.session_id(), first.session_id());
    assert_eq!(roles(&resumed.messages()), ["system", "user", "assistant", "user", "assistant"]);
    assert_eq!(rebound.lock().unwrap().clone(), [fresh.session_id(), first.session_id()]);
    runtime.dispose().await;
}

#[tokio::test]
async fn forks_before_a_user_message_and_clones_the_branch() {
    let (_h, runtime) = runtime_with_turns(true).await;
    let original = runtime.session().session_file().unwrap();
    let second = user_ids(&runtime)[1].clone();

    let forked = runtime.fork(&second, ForkPosition::Before).await.unwrap();
    assert_eq!(forked.selected_text.as_deref(), Some("second question"));
    let session = runtime.session();
    assert_ne!(session.session_file().unwrap(), original);
    assert_eq!(roles(&session.messages()), ["system", "user", "assistant"]);
    let header = session.session_manager().header_raw().clone();
    assert_eq!(header["parentSession"].as_str().unwrap(), original.display().to_string());

    let cloned = runtime.clone_session().await.unwrap();
    assert_eq!(cloned.selected_text, None);
    assert_eq!(roles(&runtime.session().messages()), ["system", "user", "assistant"]);
    let error = runtime.fork("missing", ForkPosition::At).await.unwrap_err();
    assert_eq!(error.to_string(), "Invalid entry ID for forking");
}

#[tokio::test]
async fn forks_in_memory_sessions() {
    let (_h, runtime) = runtime_with_turns(false).await;
    let first = user_ids(&runtime)[0].clone();
    let result = runtime.fork(&first, ForkPosition::Before).await.unwrap();
    assert_eq!(result.selected_text.as_deref(), Some("first question"));
    assert!(runtime.session().session_file().is_none());
    assert!(runtime.session().messages().iter().all(|message| message.role() == "system"));
    runtime.session().prompt("again", Vec::new()).await.unwrap();
    assert_eq!(text(runtime.session().messages().last().unwrap()), "third");
}

#[tokio::test]
async fn reports_unsaved_sessions_and_imports_jsonl_files() {
    let (h, runtime) = runtime_with_turns(true).await;
    let source = runtime.session().session_file().unwrap();
    let copy = h.cwd.join("exported.jsonl");
    std::fs::copy(&source, &copy).unwrap();

    runtime.new_session(None, None).await.unwrap();
    let leaf = runtime.session().session_manager().leaf_id().unwrap().to_string();
    let error = runtime.fork(&leaf, ForkPosition::At).await.unwrap_err();
    assert_eq!(error.to_string(), "This session has not been saved yet. Send a message before cloning or forking it.");

    let missing = runtime.import_from_jsonl(&h.cwd.join("nope.jsonl"), None).await.unwrap_err();
    assert!(missing.to_string().starts_with("File not found: "));
    runtime.import_from_jsonl(&copy, None).await.unwrap();
    let imported = runtime.session();
    assert_eq!(roles(&imported.messages()), ["system", "user", "assistant", "user", "assistant"]);
    let file = imported.session_file().unwrap();
    assert!(file.starts_with(h.agent_dir.join("sessions")));
    assert_eq!(imported.cwd(), h.cwd.as_path());
}

/// A login whose token endpoint answers a refresh when the test says so.
struct RotatingOAuth {
    started: Mutex<Option<oneshot::Sender<()>>>,
    response: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait]
impl OAuthAuth for RotatingOAuth {
    fn name(&self) -> &str {
        "Rotating"
    }

    async fn login(
        &self,
        _interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        _cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        Err(AuthError::other("unused"))
    }

    async fn refresh(&self, _credential: &OAuthCredential, _cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        // From here on the token endpoint has spent the old refresh token.
        if let Some(started) = self.started.lock().unwrap().take() {
            let _ = started.send(());
        }
        if let Some(response) = self.response.lock().await.take() {
            let _ = response.await;
        }
        Ok(OAuthCredential {
            refresh: "new-refresh".into(),
            access: "new".into(),
            expires: i64::MAX,
            extra: Default::default(),
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}

/// Starts a refresh of an expired login whose caller is gone before the token endpoint answers,
/// optionally replaces the session, then checks that `dispose` waits until the new token is saved.
async fn dispose_waits_for_a_refresh_in_flight(replace_session: bool) {
    let h = harness(Options::default()).await;
    // A file store, as pi uses: reads do not wait for the lock a refresh holds.
    let path = h.agent_dir.join("rotating-auth.json");
    let expired = json!({"rotating": {"type": "oauth", "refresh": "old-refresh", "access": "old", "expires": 0}});
    std::fs::write(&path, expired.to_string()).unwrap();
    let store = Arc::new(FileCredentialStore::new(path));
    let (started, refresh_started) = oneshot::channel();
    let (respond, response) = oneshot::channel();
    let oauth: Arc<dyn OAuthAuth> = Arc::new(RotatingOAuth {
        started: Mutex::new(Some(started)),
        response: tokio::sync::Mutex::new(Some(response)),
    });
    let options = AppOptions {
        credentials: Some(store.clone()),
        oauth: Some(Arc::new(move |id: &str| (id == "rotating").then(|| oauth.clone()))),
        ..h.app_options()
    };
    let runtime = crate::offline_guard::app_runtime(options, SessionManager::in_memory(&h.cwd)).await;
    let resolver = runtime.session().model_runtime().resolver().clone();
    let caller = tokio::spawn(async move { resolver.resolve("rotating", &AuthOverrides::default()).await });
    refresh_started.await.unwrap();
    caller.abort();
    if replace_session {
        runtime.new_session(None, None).await.unwrap();
    }

    let dispose = tokio::spawn(async move { runtime.dispose().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!dispose.is_finished(), "dispose returned while the refresh was in flight");
    respond.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), dispose).await.unwrap().unwrap();
    let Some(Credential::OAuth(saved)) = store.read("rotating").await.unwrap() else { panic!("the login is gone") };
    assert_eq!(saved.refresh, "new-refresh");
}

// Upstream #11692: a client that stops pi right after a probe must not lose a rotated token.
#[tokio::test]
async fn dispose_waits_until_an_oauth_refresh_in_flight_is_saved() {
    dispose_waits_for_a_refresh_in_flight(false).await;
}

// A replaced session has its own model runtime, whose refresh must be saved as well.
#[tokio::test]
async fn dispose_waits_for_a_refresh_that_a_replaced_session_started() {
    dispose_waits_for_a_refresh_in_flight(true).await;
}
