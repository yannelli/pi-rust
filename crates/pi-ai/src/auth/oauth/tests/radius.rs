use std::sync::Mutex;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::support::{
    MockServer, Recorded, Reply, TestInteraction, answer, expect_err, free_port, get, param, sequence,
};
use crate::auth::oauth::{RadiusOAuth, builtin_oauth, load_radius_gateway_config};
use crate::auth::{AuthEvent, AuthPrompt, LoginOptions, OAuthAuth, OAuthCredential};

fn select(method: &'static str) -> TestInteraction {
    TestInteraction::new(move |prompt, _, _| match prompt {
        AuthPrompt::Select { message, options } => {
            assert_eq!(message, "Sign in to Radius:");
            let ids: Vec<_> = options.iter().map(|option| option.id.as_str()).collect();
            assert_eq!(ids, ["browser", "device-code"]);
            answer(method)
        }
        other => panic!("unexpected prompt {other:?}"),
    })
}

async fn flow(server: &MockServer) -> RadiusOAuth {
    let mut flow = RadiusOAuth::new("Radius", &format!("{}/", server.base));
    flow.callback_port = free_port().await;
    flow.hooks.no_timeouts = true;
    flow
}

#[tokio::test(start_paused = true)]
async fn device_login_uses_gateway_endpoints_directly() {
    let poll = sequence(vec![
        (400, json!({ "error": "authorization_pending" })),
        (400, json!({ "error": "slow_down" })),
        (200, json!({ "access_token": "access", "refresh_token": "refresh", "expires_in": 3600, "scope": "gateway" })),
    ]);
    let server = MockServer::start(move |request: &Recorded| match request.path.as_str() {
        "/v1/oauth/device" => {
            assert_eq!(request.form("client_id").as_deref(), Some("pi-gateway"));
            assert_eq!(request.form("scope").as_deref(), Some("gateway offline_access"));
            Reply::Json(
                200,
                json!({ "device_code": "dc", "user_code": "UC", "verification_uri": "https://r/device",
                "expires_in": 600, "interval": 2 }),
            )
        }
        "/v1/oauth/token" => {
            assert_eq!(request.form("grant_type").as_deref(), Some("urn:ietf:params:oauth:grant-type:device_code"));
            assert_eq!(request.form("device_code").as_deref(), Some("dc"));
            poll()
        }
        other => panic!("unexpected request {other}"),
    })
    .await;
    let interaction = select("device-code");
    let start = tokio::time::Instant::now();
    let credential = flow(&server).await.login(&interaction, &LoginOptions::default(), CancellationToken::new()).await;
    let credential = credential.unwrap();
    assert_eq!((credential.access.as_str(), credential.extra_str("scope")), ("access", Some("gateway")));
    let times: Vec<u128> = server.requests_to("/token").iter().map(|r| (r.at - start).as_millis()).collect();
    assert_eq!(times, [0, 2000, 9000]);
    assert_eq!(
        interaction.events(),
        [AuthEvent::DeviceCode {
            user_code: "UC".into(),
            verification_uri: "https://r/device".into(),
            interval_seconds: Some(2),
            expires_in_seconds: Some(600),
        }]
    );
}

#[tokio::test(start_paused = true)]
async fn device_login_maps_oauth_errors() {
    let cases = [
        (json!({ "error": "expired_token" }), "Device authorization expired."),
        (json!({ "error": "access_denied" }), "Device authorization was denied."),
        (
            json!({ "error": "server_error", "error_description": "down" }),
            "Radius OAuth token request failed: server_error: down",
        ),
    ];
    for (reply, message) in cases {
        let server = MockServer::start(move |request: &Recorded| match request.path.as_str() {
            "/v1/oauth/device" => Reply::Json(
                200,
                json!({ "device_code": "d", "user_code": "u", "verification_uri": "https://r/device", "expires_in": 60 }),
            ),
            _ => Reply::Json(400, reply.clone()),
        })
        .await;
        let result =
            flow(&server).await.login(&select("device-code"), &LoginOptions::default(), CancellationToken::new()).await;
        assert_eq!(expect_err(result), message);
    }
    let server = MockServer::start(|_: &Recorded| Reply::Text(503, "maintenance".into())).await;
    let result =
        flow(&server).await.login(&select("device-code"), &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "Radius OAuth device authorization failed: maintenance");
}

#[tokio::test]
async fn logins_reject_server_urls_that_are_not_http() {
    let payload = "file:///Applications/payload.app";
    let server = MockServer::start(move |request: &Recorded| match request.path.as_str() {
        "/v1/oauth" => Reply::Json(200, json!({ "authorizationEndpoint": payload })),
        "/v1/oauth/device" => Reply::Json(
            200,
            json!({ "device_code": "device-code", "user_code": "ABCD-1234", "verification_uri": payload, "expires_in": 600 }),
        ),
        other => panic!("unexpected request {other}"),
    })
    .await;
    let browser = select("browser");
    let result = flow(&server).await.login(&browser, &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), format!("Invalid Radius OAuth config from {}", server.base));
    assert_eq!(browser.events(), []);
    let device = select("device-code");
    let result = flow(&server).await.login(&device, &LoginOptions::default(), CancellationToken::new()).await;
    assert!(expect_err(result).contains("Untrusted verification_uri"));
    assert_eq!(device.events(), []);
}

#[tokio::test]
async fn refresh_goes_straight_to_the_gateway() {
    let server = MockServer::start(|request: &Recorded| {
        assert_eq!(request.path, "/v1/oauth/token");
        assert_eq!(request.form("grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(request.form("client_id").as_deref(), Some("pi-gateway"));
        assert_eq!(request.form("refresh_token").as_deref(), Some("old-refresh"));
        Reply::Json(200, json!({ "access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600 }))
    })
    .await;
    let old = OAuthCredential { refresh: "old-refresh".into(), ..Default::default() };
    let oauth = flow(&server).await;
    let refreshed = oauth.refresh(&old, CancellationToken::new()).await.unwrap();
    assert_eq!((refreshed.access.as_str(), refreshed.refresh.as_str()), ("new-access", "new-refresh"));
    assert!(refreshed.extra.get("scope").is_none());
    let expected = crate::auth::oauth::http::now_ms() + 3_600_000 - 60_000;
    assert!((refreshed.expires - expected).abs() < 5_000);
    assert_eq!(server.requests().len(), 1);
    assert_eq!(oauth.to_auth(&refreshed).api_key.as_deref(), Some("new-access"));
}

#[tokio::test]
async fn browser_login_requires_discovery_and_exchanges_before_the_page() {
    let server = MockServer::start(|_: &Recorded| Reply::Json(200, json!({ "issuer": "https://ui" }))).await;
    let oauth = flow(&server).await;
    let result = oauth.login(&select("browser"), &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), format!("Invalid Radius OAuth config from {}", server.base));

    let token_status = std::sync::Arc::new(Mutex::new(400u16));
    let status = token_status.clone();
    let base = std::sync::Arc::new(Mutex::new(String::new()));
    let authorize = base.clone();
    let server = MockServer::start(move |request: &Recorded| match request.path.as_str() {
        "/v1/oauth" => {
            Reply::Json(200, json!({ "authorizationEndpoint": format!("{}/authorize?x=1", authorize.lock().unwrap()) }))
        }
        "/v1/oauth/token" => {
            assert_eq!(request.form("code").as_deref(), Some("browser-code"));
            match *status.lock().unwrap() {
                200 => Reply::Json(
                    200,
                    json!({ "access_token": "access", "refresh_token": "refresh", "expires_in": 3600 }),
                ),
                code => Reply::Json(code, json!({ "error": "invalid_grant", "error_description": "code expired" })),
            }
        }
        other => panic!("unexpected request {other}"),
    })
    .await;
    *base.lock().unwrap() = server.base.clone();
    let oauth = flow(&server).await;
    let login = || async {
        let (sender, page) = tokio::sync::oneshot::channel();
        let sender = Mutex::new(Some(sender));
        let interaction = select("browser").on_notify(move |event| {
            let AuthEvent::AuthUrl { url, .. } = event else { return };
            let url = reqwest::Url::parse(url).unwrap();
            assert_eq!(param(&url, "x"), None);
            assert_eq!(param(&url, "handoff").as_deref(), Some("url"));
            let target = format!(
                "{}?code=browser-code&state={}",
                param(&url, "redirect_uri").unwrap(),
                param(&url, "state").unwrap()
            );
            let sender = sender.lock().unwrap().take().unwrap();
            tokio::spawn(async move { sender.send(get(&target).await.unwrap()) });
        });
        let result = oauth.login(&interaction, &LoginOptions::default(), CancellationToken::new()).await;
        (result, page.await.unwrap(), interaction.events())
    };
    let (failed, (status, page), _) = login().await;
    assert!(expect_err(failed).contains("invalid_grant: code expired"));
    assert_eq!((status, page.contains("code expired")), (502, true));

    *token_status.lock().unwrap() = 200;
    let (succeeded, (status, page), events) = login().await;
    assert_eq!(succeeded.unwrap().access, "access");
    assert_eq!((status, page.contains("Signed in to Radius.")), (200, true));
    let redirect = format!("http://127.0.0.1:{}/oauth/callback", oauth.callback_port);
    assert_eq!(events[0], AuthEvent::Progress { message: format!("Listening for OAuth callback on {redirect}") });
}

#[tokio::test]
async fn gateway_catalog_loads_with_the_access_token() {
    let server = MockServer::start(|request: &Recorded| {
        assert_eq!((request.path.as_str(), request.header("authorization")), ("/v1/config", Some("Bearer key")));
        let model = json!({ "id": "m", "name": "M", "reasoning": true, "input": ["text"], "cost": { "input": 1, "output": 2 },
            "contextWindow": 1000, "maxTokens": 10 });
        Reply::Json(200, json!({ "baseUrl": "https://gw/v1", "models": [model, { "id": 1 }] }))
    })
    .await;
    let config = load_radius_gateway_config(&server.base, Some("key"), &CancellationToken::new()).await.unwrap();
    assert_eq!((config.base_url.as_str(), config.models.len()), ("https://gw/v1", 1));
    assert_eq!(config.models[0]["id"], Value::from("m"));
    let failing = MockServer::start(|_: &Recorded| Reply::Text(500, format!("  {}  ", "x".repeat(600)))).await;
    let error = expect_err(load_radius_gateway_config(&failing.base, None, &CancellationToken::new()).await);
    assert_eq!(error, format!("Could not load Radius config from {}: 500: {}…", failing.base, "x".repeat(512)));
    assert_eq!(builtin_oauth("radius").unwrap().name(), "Radius");
}
