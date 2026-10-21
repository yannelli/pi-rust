//! llama.cpp server login through the dialog against a local mock server.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use pi_ai::auth::{AuthResult, AuthType, Credential, CredentialStore, InMemoryCredentialStore, LoginOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::agent_session::{ModelRuntime, ModelRuntimeOptions};
use crate::interactive_components::login::{LoginDialog, llama_cpp_post_login_guidance, run_dialog_login};
use crate::interactive_components::test_support::{Harness, flush, keys, local};

/// Serves one canned response per connection; records request heads.
async fn mock_server(status: &'static str, body: &'static str) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen = requests.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = vec![0u8; 4096];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            seen.lock().unwrap().push(String::from_utf8_lossy(&buffer[..read]).into_owned());
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (format!("http://{address}"), requests)
}

#[test]
fn post_login_guidance_depends_on_loaded_models() {
    assert_eq!(
        llama_cpp_post_login_guidance("Saved API key for llama.cpp", 0),
        "Saved API key for llama.cpp. No llama.cpp models are loaded. Use /llama to load a model, then /model to select it."
    );
    assert_eq!(
        llama_cpp_post_login_guidance("Saved API key for llama.cpp", 2),
        "Saved API key for llama.cpp. Use /model to select a loaded llama.cpp model, or /llama to manage models."
    );
}

#[tokio::test(flavor = "current_thread")]
async fn llama_login_prompts_for_url_and_key_then_stores_the_server() {
    let (url, requests) = mock_server("200 OK", r#"{"data":[{"id":"qwen","status":{"value":"loaded"}}]}"#).await;
    local(async move {
        let store = Arc::new(InMemoryCredentialStore::new(BTreeMap::new()));
        let dir = std::env::temp_dir().join("pi-llama-login-test-missing");
        let options = ModelRuntimeOptions {
            models_path: dir.join("models.json"),
            credentials: store.clone(),
            oauth: None,
            credential_env: Arc::new(|_: &str| None::<String>),
        };
        let runtime = Arc::new(ModelRuntime::create(options).await);
        let harness =
            Harness::new(100, 30, |tui| LoginDialog::new(tui, "llama.cpp", |_, _: Option<&str>| {}, None, None));
        let result: Rc<RefCell<Option<AuthResult<Credential>>>> = Rc::default();
        let (dialog, slot) = (harness.component.clone(), result.clone());
        tokio::task::spawn_local(async move {
            let options = LoginOptions::default();
            let outcome = run_dialog_login(&dialog, &runtime, "llama.cpp", AuthType::ApiKey, &options).await;
            *slot.borrow_mut() = Some(outcome);
        });
        flush(&harness).await;
        let screen = harness.screen();
        assert!(screen.contains("Login to llama.cpp") && screen.contains("llama.cpp server URL"), "{screen}");
        assert!(screen.contains("e.g., "));
        harness.type_text(&format!("{url}/v1/"));
        harness.send(keys::ENTER);
        flush(&harness).await;
        assert!(harness.screen().contains("API key (optional)"));
        harness.type_text("secret");
        harness.send(keys::ENTER);
        for _ in 0..50 {
            if result.borrow().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let Credential::ApiKey(credential) = result.borrow_mut().take().expect("finished").unwrap() else {
            panic!("api key credential")
        };
        assert_eq!(credential.key.as_deref(), Some("secret"));
        assert_eq!(credential.env.unwrap()["LLAMA_BASE_URL"], url);
        assert!(store.read("llama.cpp").await.unwrap().is_some());
        let request = requests.lock().unwrap()[0].clone();
        assert!(request.starts_with("GET /models "), "{request}");
        assert!(request.to_lowercase().contains("authorization: bearer secret"));
    })
    .await;
}
