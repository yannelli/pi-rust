use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::support::{MockServer, Recorded, Reply, TestInteraction, answer, expect_err, get, param, pending};
use crate::auth::oauth::{OpenRouterOAuth, pkce_challenge};
use crate::auth::{AuthError, AuthEvent, LoginOptions, OAuthAuth, OAuthCredential};

const NEVER_EXPIRES: i64 = 9_007_199_254_740_991;

fn flow(server: &MockServer) -> OpenRouterOAuth {
    OpenRouterOAuth { hooks: server.hooks() }
}

fn callback_url(event: &AuthEvent) -> Option<reqwest::Url> {
    let AuthEvent::AuthUrl { url, .. } = event else { return None };
    reqwest::Url::parse(&param(&reqwest::Url::parse(url).unwrap(), "callback_url").unwrap()).ok()
}

/// Hits the loopback callback with `code` as soon as the auth URL is shown.
fn browser(code: &'static str) -> (TestInteraction, oneshot::Receiver<(u16, String)>) {
    let (sender, page) = oneshot::channel();
    let sender = Mutex::new(Some(sender));
    let interaction = TestInteraction::new(|_, cancel, _| pending(cancel)).on_notify(move |event| {
        let Some(mut url) = callback_url(event) else { return };
        url.query_pairs_mut().append_pair("code", code);
        let sender = sender.lock().unwrap().take().unwrap();
        tokio::spawn(async move { sender.send(get(url.as_str()).await.unwrap()) });
    });
    (interaction, page)
}

#[tokio::test]
async fn browser_callback_exchanges_the_code_for_a_permanent_key() {
    let server = MockServer::start(|request: &Recorded| {
        assert_eq!(request.path, "/openrouter.ai/api/v1/auth/keys");
        Reply::Json(200, json!({ "key": "sk-or-test" }))
    })
    .await;
    let (interaction, page) = browser("authorization-code");
    let credential =
        flow(&server).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await.unwrap();
    let expected = OAuthCredential {
        access: "sk-or-test".into(),
        refresh: String::new(),
        expires: NEVER_EXPIRES,
        ..Default::default()
    };
    assert_eq!(credential, expected);
    assert_eq!(page.await.unwrap().0, 200);
    assert!(interaction.prompts.lock().unwrap()[0].1.is_cancelled());

    let url = interaction.auth_url();
    assert_eq!(url.as_str().split('?').next(), Some("https://openrouter.ai/auth"));
    assert_eq!(param(&url, "code_challenge_method").as_deref(), Some("S256"));
    let callback = reqwest::Url::parse(&param(&url, "callback_url").unwrap()).unwrap();
    assert_eq!(callback.host_str(), Some("127.0.0.1"));
    let uuid = callback.path().strip_prefix("/oauth/callback/").unwrap();
    assert!(uuid.len() == 36 && uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    let body = server.requests()[0].json();
    assert_eq!(
        (body["code"].as_str(), body["code_challenge_method"].as_str()),
        (Some("authorization-code"), Some("S256"))
    );
    assert_eq!(param(&url, "code_challenge").unwrap(), pkce_challenge(body["code_verifier"].as_str().unwrap()));
    let progress = AuthEvent::Progress { message: format!("Listening for OpenRouter OAuth callback on {callback}") };
    assert_eq!(interaction.events()[0], progress);
}

#[tokio::test]
async fn exchange_failures_reach_both_the_page_and_the_login() {
    let server =
        MockServer::start(|_: &Recorded| Reply::Json(403, json!({ "error": { "message": "invalid code" } }))).await;
    let (interaction, page) = browser("bad-code");
    let result = flow(&server).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "OpenRouter OAuth key exchange failed (HTTP 403): invalid code");
    assert_eq!(page.await.unwrap().0, 502);

    let keyless = MockServer::start(|_: &Recorded| Reply::Json(200, json!({ "data": {} }))).await;
    let (interaction, _page) = browser("code");
    let result = flow(&keyless).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), r#"OpenRouter OAuth response carries no "key""#);
}

#[tokio::test]
async fn only_one_callback_exchanges_a_code() {
    let (release, released) = oneshot::channel();
    let released = Mutex::new(Some(released));
    let server =
        MockServer::start(move |_: &Recorded| Reply::Later(released.lock().unwrap().take().expect("one exchange")))
            .await;
    let callback = Arc::new(Mutex::new(None));
    let seen = callback.clone();
    let interaction = TestInteraction::new(|_, cancel, _| pending(cancel)).on_notify(move |event| {
        if let Some(mut url) = callback_url(event) {
            url.query_pairs_mut().append_pair("code", "authorization-code");
            *seen.lock().unwrap() = Some(url.to_string());
        }
    });
    let (flow, options) = (flow(&server), LoginOptions::default());
    let login = flow.login(&interaction, &options, CancellationToken::new());
    let driver = async {
        let url = loop {
            if let Some(url) = callback.lock().unwrap().clone() {
                break url;
            }
            tokio::task::yield_now().await;
        };
        let first = tokio::spawn({
            let url = url.clone();
            async move { get(&url).await.unwrap().0 }
        });
        while server.requests().is_empty() {
            tokio::task::yield_now().await;
        }
        assert_eq!(get(&url).await.unwrap().0, 409);
        release.send(Reply::Json(200, json!({ "key": "sk-or-test" }))).ok();
        first.await.unwrap()
    };
    let (credential, first_status) = tokio::join!(login, driver);
    assert_eq!((credential.unwrap().access.as_str(), first_status), ("sk-or-test", 200));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn pasted_redirects_and_bare_codes_mint_keys() {
    let server = MockServer::start(|_: &Recorded| Reply::Json(200, json!({ "key": "sk-or-pasted" }))).await;
    for (input, code) in
        [("http://elsewhere/cb?code=pasted-code", "pasted-code"), ("bare-code", "bare-code"), ("code=q&x=1", "q")]
    {
        let interaction = TestInteraction::new(move |_, _, _| answer(input));
        let credential = flow(&server).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await;
        assert_eq!(credential.unwrap().access, "sk-or-pasted");
        assert_eq!(server.requests().last().unwrap().json()["code"], code);
        let last = interaction.events().pop().unwrap();
        assert_eq!(last, AuthEvent::Progress { message: "Exchanging authorization code for an API key...".into() });
    }
    let requests = server.requests().len();
    let empty = TestInteraction::new(|_, _, _| answer("   "));
    let result = flow(&server).login(&empty, &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "Missing authorization code");
    let refused = TestInteraction::new(|_, _, _| Box::pin(async { Err(AuthError::Cancelled) }));
    let result = flow(&server).login(&refused, &LoginOptions::default(), CancellationToken::new()).await;
    assert!(matches!(result, Err(AuthError::Cancelled)));
    assert_eq!(server.requests().len(), requests);
}

#[tokio::test]
async fn cancellation_closes_the_callback_and_honors_the_callback_host() {
    let server = MockServer::start(|_: &Recorded| panic!("no exchange")).await;
    let cancel = CancellationToken::new();
    let (stop, seen) = (cancel.clone(), Arc::new(Mutex::new(None)));
    let callback = seen.clone();
    let interaction = TestInteraction::new(|_, cancel, _| pending(cancel)).on_notify(move |event| {
        if let Some(url) = callback_url(event) {
            *callback.lock().unwrap() = Some(url);
            stop.cancel();
        }
    });
    let mut hooks = server.hooks();
    hooks.env = Some(Arc::new(|name| (name == "PI_OAUTH_CALLBACK_HOST").then(|| "localhost".into())));
    let result = OpenRouterOAuth { hooks }.login(&interaction, &LoginOptions::default(), cancel).await;
    assert!(matches!(result, Err(AuthError::Cancelled)), "{result:?}");
    let url = seen.lock().unwrap().clone().unwrap();
    assert_eq!(url.host_str(), Some("localhost"));
    tokio::task::yield_now().await;
    assert!(get(url.as_str()).await.is_err(), "the callback server is closed");

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let silent =
        TestInteraction::new(|_, _, _| answer("")).on_notify(|_| panic!("cancelled login must not emit events"));
    let result = flow(&server).login(&silent, &LoginOptions::default(), cancelled).await;
    assert!(matches!(result, Err(AuthError::Cancelled)));
}

#[tokio::test]
async fn refresh_keeps_the_permanent_key() {
    let oauth = OpenRouterOAuth::default();
    let credential = OAuthCredential { access: "token".into(), expires: NEVER_EXPIRES, ..Default::default() };
    assert_eq!(oauth.refresh(&credential, CancellationToken::new()).await.unwrap(), credential);
    assert_eq!(oauth.to_auth(&credential).api_key.as_deref(), Some("token"));
    assert!(!oauth.is_subscription());
    assert_eq!((oauth.name(), oauth.login_label()), ("OpenRouter OAuth", Some("Sign in with OpenRouter")));
}
