//! OAuth flow against local authorization servers. Ported from packages/mcp/test/oauth.test.ts.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::http::{Response, serve};
use common::provider::TestProvider;
use common::wait_until;
use pi_mcp::oauth::pkce::challenge_for;
use pi_mcp::oauth::{
    AuthorizationServerMetadata, CallbackServer, CallbackServerOptions, LoopbackCallbackServer, McpOAuthProvider,
    McpOAuthProviderOptions, McpOAuthStateStore, MemoryOAuthStateStore, OAuthClientInformation, OAuthClientMetadata,
    OAuthClientProvider, OAuthDiscoveryState, OAuthFlowOptions, OAuthFlowResult, OAuthHttp, OAuthProviderAuth,
    OAuthTokens, authorize_mcp, discover_authorization_server_metadata, register_client,
};
use pi_mcp::{
    AuthProvider, McpClient, McpClientOptions, McpError, OAuthError, StreamableHttpOptions, StreamableHttpTransport,
    Tool, UnauthorizedContext,
};
use reqwest::header::{HeaderMap, HeaderValue, WWW_AUTHENTICATE};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use url::Url;

fn http() -> OAuthHttp {
    OAuthHttp::new(reqwest::Client::new())
}

fn flow(server_url: &str) -> OAuthFlowOptions {
    OAuthFlowOptions::new(Url::parse(server_url).unwrap(), http())
}

fn metadata(origin: &str) -> AuthorizationServerMetadata {
    AuthorizationServerMetadata {
        issuer: origin.into(),
        authorization_endpoint: format!("{origin}/authorize"),
        token_endpoint: format!("{origin}/token"),
        response_types_supported: vec!["code".into()],
        ..AuthorizationServerMetadata::default()
    }
}

fn tokens(access: &str, refresh: &str) -> OAuthTokens {
    OAuthTokens {
        access_token: access.into(),
        refresh_token: Some(refresh.into()),
        token_type: "Bearer".into(),
        ..OAuthTokens::default()
    }
}

fn unauthorized(origin: &str, status: u16, challenge: &str, token: &str) -> UnauthorizedContext {
    let mut headers = HeaderMap::new();
    headers.insert(WWW_AUTHENTICATE, HeaderValue::from_str(challenge).unwrap());
    UnauthorizedContext {
        status,
        headers,
        server_url: Url::parse(&format!("{origin}/mcp")).unwrap(),
        client: reqwest::Client::new(),
        token: Some(token.into()),
    }
}

async fn connect(origin: &str, provider: Arc<TestProvider>) -> Result<McpClient, McpError> {
    let mut options = StreamableHttpOptions::new(Url::parse(&format!("{origin}/mcp")).unwrap());
    options.headers = vec![("Authorization".into(), "Bearer caller-supplied-stale-token".into())];
    options.auth_provider = Some(Arc::new(OAuthProviderAuth::new(provider)));
    options.open_get_stream = false;
    let client = McpClient::new(McpClientOptions::new("oauth-test", "1.0.0"));
    client.connect(Arc::new(StreamableHttpTransport::new(options)?)).await?;
    Ok(client)
}

/// The authorization and token endpoints of the first test: PKCE-checked codes and refreshes.
fn authorization_server(
    request: &common::http::Request,
    origin: &str,
    challenge: &Mutex<Option<String>>,
    refreshes: &AtomicUsize,
) -> Response {
    match request.path() {
        "/.well-known/oauth-protected-resource/mcp" => Response::json(
            200,
            &json!({ "resource": format!("{origin}/mcp"), "authorization_servers": [origin], "scopes_supported": ["org:read"] }),
        ),
        "/.well-known/oauth-authorization-server" => Response::json(
            200,
            &json!({
                "issuer": origin, "authorization_endpoint": format!("{origin}/authorize"),
                "token_endpoint": format!("{origin}/token"), "registration_endpoint": format!("{origin}/register"),
                "response_types_supported": ["code"], "grant_types_supported": ["authorization_code", "refresh_token"],
                "token_endpoint_auth_methods_supported": ["none"], "code_challenge_methods_supported": ["S256"]
            }),
        ),
        "/register" => {
            // Empty and null optional fields count as absent.
            let mut metadata = request.json();
            metadata["client_id"] = json!("test-client");
            metadata["client_secret"] = json!("");
            Response::json(201, &metadata)
        }
        "/authorize" => {
            let query = request.query();
            *challenge.lock().unwrap() = query.get("code_challenge").cloned();
            let mut redirect = Url::parse(&query["redirect_uri"]).unwrap();
            redirect.query_pairs_mut().append_pair("code", "test-code").append_pair("state", &query["state"]);
            Response::status(302).header("location", redirect.as_str())
        }
        _ => {
            let form = request.form();
            if form.get("grant_type").map(String::as_str) == Some("refresh_token") {
                refreshes.fetch_add(1, Ordering::SeqCst);
                let body = json!({ "access_token": "refreshed-token", "token_type": "Bearer", "refresh_token": "", "expires_in": null });
                return Response::json(200, &body);
            }
            let verified = Some(challenge_for(&form["code_verifier"])) == *challenge.lock().unwrap();
            if form.get("code").map(String::as_str) != Some("test-code") || !verified {
                return Response::json(400, &json!({ "error": "invalid_grant" }));
            }
            let body = json!({ "access_token": "first-token", "refresh_token": "refresh-token", "token_type": "Bearer", "scope": "" });
            Response::json(200, &body)
        }
    }
}

fn mcp_endpoint(request: &common::http::Request, origin: &str) -> Response {
    match request.method.as_str() {
        "GET" => return Response::status(405),
        "DELETE" => return Response::status(200),
        _ => {}
    }
    let token = request.header("authorization").unwrap_or_default();
    if token != "Bearer first-token" && token != "Bearer refreshed-token" {
        // An empty scope falls through to the resource metadata's scopes_supported.
        let metadata = format!("{origin}/.well-known/oauth-protected-resource/mcp");
        let challenge = format!("Bearer resource_metadata=\"{metadata}\", scope=\"\"");
        return Response::text(401, "Unauthorized").header("www-authenticate", &challenge);
    }
    let message = request.json();
    let Some(id) = message.get("id") else { return Response::status(202) };
    let result = if message["method"] == "initialize" {
        json!({ "protocolVersion": "2025-06-18", "capabilities": { "tools": {} }, "serverInfo": { "name": "oauth-test", "version": "1.0.0" } })
    } else {
        json!({ "tools": [{ "name": "issues", "inputSchema": { "type": "object" } }] })
    };
    Response::json(200, &json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

#[tokio::test]
async fn discovers_registers_authorizes_with_pkce_and_refreshes_on_401() {
    let challenge = Arc::new(Mutex::new(None::<String>));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let (expected, refresh_count) = (challenge.clone(), refreshes.clone());
    let origin = serve(move |request, origin| {
        let response = match request.path() {
            "/mcp" => mcp_endpoint(&request, &origin),
            "/.well-known/oauth-protected-resource/mcp"
            | "/.well-known/oauth-authorization-server"
            | "/register"
            | "/authorize"
            | "/token" => authorization_server(&request, &origin, &expected, &refresh_count),
            _ => Response::status(404),
        };
        async { response }
    })
    .await;

    let callback = LoopbackCallbackServer::listen(CallbackServerOptions::default()).await.unwrap();
    let provider = TestProvider::new(&callback.redirect_url());
    let error = connect(&origin, provider.clone()).await.unwrap_err();
    assert!(matches!(error, McpError::OAuth(OAuthError::AuthorizationRequired)), "{error:?}");
    assert_eq!(provider.authorization_param("scope").as_deref(), Some("org:read"));
    assert_eq!(provider.authorization_param("resource"), Some(format!("{origin}/mcp")));

    let callback_result = callback.wait_for_callback("expected-state", None).unwrap();
    let authorization_url = provider.saved().authorization_url.clone().unwrap();
    let manual = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let response = manual.get(authorization_url).send().await.unwrap();
    let location = response.headers()["location"].to_str().unwrap().to_string();
    reqwest::get(location).await.unwrap();
    let code = callback_result.await.unwrap().code;
    let mut options = flow(&format!("{origin}/mcp"));
    options.authorization_code = Some(code);
    assert_eq!(authorize_mcp(provider.as_ref(), &options).await.unwrap(), OAuthFlowResult::Authorized);

    let client = connect(&origin, provider.clone()).await.unwrap();
    let tools = client.list_tools(Default::default()).await.unwrap();
    assert_eq!(tools, [Tool::new("issues", json!({ "type": "object" }))]);
    client.close().await;

    provider.saved().tokens.as_mut().unwrap().access_token = "stale-token".into();
    let refreshed = connect(&origin, provider.clone()).await.unwrap();
    // Neither token response names a scope, so the grant has the requested scope.
    let expected = OAuthTokens { scope: Some("org:read".into()), ..tokens("refreshed-token", "refresh-token") };
    assert_eq!(provider.saved().tokens, Some(expected));
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    refreshed.close().await;
    callback.close().await;
}

fn provider_for(server_url: &str, client_id: Option<&str>, store: Arc<dyn McpOAuthStateStore>) -> McpOAuthProvider {
    McpOAuthProvider::new(McpOAuthProviderOptions {
        server_url: Url::parse(server_url).unwrap(),
        redirect_url: "http://127.0.0.1/callback".into(),
        client_metadata: OAuthClientMetadata { client_name: Some("test".into()), ..OAuthClientMetadata::default() },
        client_metadata_document: None,
        client_id: client_id.map(str::to_string),
        client_secret: None,
        store,
        on_redirect: Arc::new(|_| {}),
    })
}

#[tokio::test]
async fn shares_one_refresh_between_concurrent_401s_when_refresh_tokens_rotate() {
    let grants = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = grants.clone();
    let origin = serve(move |request, origin| {
        let seen = seen.clone();
        async move {
            match request.path() {
                // Invalid resource metadata falls back to the server origin instead of failing discovery.
                "/.well-known/oauth-protected-resource/mcp" => Response::json(
                    200,
                    &json!({ "resource": format!("{origin}/mcp"), "authorization_servers": ["not a url"] }),
                ),
                "/.well-known/oauth-authorization-server" => {
                    Response::json(200, &serde_json::to_value(metadata(&origin)).unwrap())
                }
                "/token" => {
                    let refresh_token = request.form().get("refresh_token").cloned().unwrap_or_default();
                    seen.lock().unwrap().push(refresh_token.clone());
                    if refresh_token != "r1" {
                        return Response::json(400, &json!({ "error": "invalid_grant" }));
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let body = json!({ "access_token": "a2", "refresh_token": "r2", "token_type": "Bearer", "expires_in": 3600 });
                    Response::json(200, &body)
                }
                _ => Response::status(404),
            }
        }
    })
    .await;
    let store = Arc::new(MemoryOAuthStateStore::default());
    let provider = Arc::new(provider_for(&format!("{origin}/mcp"), Some("client"), store.clone()));
    provider.save_tokens(tokens("a1", "r1")).await.unwrap();
    let auth = OAuthProviderAuth::new(provider);
    let (first, second) = futures::join!(
        auth.on_unauthorized(unauthorized(&origin, 401, "Bearer", "a1")),
        auth.on_unauthorized(unauthorized(&origin, 401, "Bearer", "a1"))
    );
    first.unwrap();
    second.unwrap();
    // A late 401 for a request that still carried the old token must not refresh again.
    auth.on_unauthorized(unauthorized(&origin, 401, "Bearer", "a1")).await.unwrap();
    assert_eq!(*grants.lock().unwrap(), ["r1"]);
    assert_eq!(auth.token().await.unwrap().as_deref(), Some("a2"));
    let state = store.load().await.unwrap().unwrap();
    assert_eq!(state.tokens.unwrap().refresh_token.as_deref(), Some("r2"));
    assert!(state.tokens_expire_at.unwrap() > pi_ai::now_ms() + 3_500_000);
}

#[tokio::test]
async fn asks_for_authorization_instead_of_refreshing_when_the_server_needs_more_scope() {
    let provider = TestProvider::new("http://127.0.0.1/callback");
    provider.saved().client = Some(OAuthClientInformation::new("client"));
    provider.saved().tokens = Some(OAuthTokens { scope: Some("repo read:org".into()), ..tokens("a1", "r1") });
    let origin = serve(|request, origin| async move {
        match request.path() {
            "/.well-known/oauth-authorization-server" => {
                Response::json(200, &serde_json::to_value(metadata(&origin)).unwrap())
            }
            "/token" => Response::status(500),
            _ => Response::status(404),
        }
    })
    .await;
    let auth = OAuthProviderAuth::new(provider.clone());
    let challenge = "Bearer error=\"insufficient_scope\", scope=\"repo admin\"";
    let error = auth.on_unauthorized(unauthorized(&origin, 403, challenge, "a1")).await.unwrap_err();
    assert!(matches!(error, McpError::OAuth(OAuthError::AuthorizationRequired)), "{error:?}");
    // The challenge may list only the missing scopes; the new grant keeps the old ones too.
    assert_eq!(provider.authorization_param("scope").as_deref(), Some("repo read:org admin"));
    // The working grant is kept until the user authorizes the new scope.
    assert_eq!(provider.saved().tokens.as_ref().unwrap().access_token, "a1");
}

#[tokio::test]
async fn binds_persisted_credentials_to_the_exact_mcp_server_url() {
    let store: Arc<dyn McpOAuthStateStore> = Arc::new(MemoryOAuthStateStore::default());
    let first = provider_for("https://one.example/mcp", None, store.clone());
    let saved = OAuthTokens { access_token: "secret".into(), token_type: "Bearer".into(), ..OAuthTokens::default() };
    first.save_tokens(saved).await.unwrap();
    assert_eq!(first.tokens().await.unwrap().unwrap().access_token, "secret");
    let second = provider_for("https://two.example/mcp", None, store);
    assert_eq!(second.tokens().await.unwrap(), None);
}

#[tokio::test]
async fn registers_with_an_application_type_derived_from_the_redirect_uris_unless_one_is_set() {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    let origin = serve(move |request, _| {
        let mut metadata = request.json();
        seen.lock().unwrap().push(metadata["application_type"].clone());
        metadata["client_id"] = json!("client");
        async move { Response::json(201, &metadata) }
    })
    .await;
    let cases = [
        ("http://127.0.0.1:1234/callback", None),
        ("http://[::1]/callback", None),
        ("com.example.app:/callback", None),
        ("https://app.example/callback", None),
        ("http://localhost/callback", Some("web")),
    ];
    for (redirect, application_type) in cases {
        let metadata = OAuthClientMetadata {
            redirect_uris: vec![redirect.into()],
            application_type: application_type.map(str::to_string),
            ..OAuthClientMetadata::default()
        };
        register_client(&origin, None, &metadata, None, &http()).await.unwrap();
    }
    let expected = [json!("native"), json!("native"), json!("native"), json!("web"), json!("web")];
    assert_eq!(*bodies.lock().unwrap(), expected);
}

#[tokio::test]
async fn rejects_authorization_metadata_whose_issuer_does_not_match_discovery() {
    let origin = serve(|request, origin| async move {
        if request.path() != "/.well-known/oauth-authorization-server" {
            return Response::status(404);
        }
        let metadata = AuthorizationServerMetadata { issuer: "https://attacker.example".into(), ..metadata(&origin) };
        Response::json(200, &serde_json::to_value(metadata).unwrap())
    })
    .await;
    let error = discover_authorization_server_metadata(&origin, &http(), false).await.unwrap_err();
    assert!(matches!(error, OAuthError::IssuerMismatch { .. }), "{error:?}");
}

#[tokio::test]
async fn uses_a_configured_authorization_server_metadata_document_as_is() {
    let origin = serve(|request, origin| async move {
        match request.path() {
            // Names the MCP server itself, which serves no authorization server metadata.
            "/.well-known/oauth-protected-resource/mcp" => {
                Response::json(200, &json!({ "resource": format!("{origin}/mcp"), "authorization_servers": [origin] }))
            }
            "/idp/metadata.json" => Response::json(
                200,
                &json!({
                    "issuer": "https://idp.example", "authorization_endpoint": format!("{origin}/idp/authorize"),
                    "token_endpoint": format!("{origin}/idp/token"), "response_types_supported": ["code"]
                }),
            ),
            _ => Response::status(404),
        }
    })
    .await;
    let provider = TestProvider::new("http://127.0.0.1/callback");
    provider.saved().client = Some(OAuthClientInformation::new("client"));
    let mut options = flow(&format!("{origin}/mcp"));
    options.authorization_server_metadata_url = Some(Url::parse(&format!("{origin}/idp/metadata.json")).unwrap());
    assert_eq!(authorize_mcp(provider.as_ref(), &options).await.unwrap(), OAuthFlowResult::Redirect);
    let url = provider.saved().authorization_url.clone().unwrap();
    assert_eq!(format!("{}{}", url.origin().ascii_serialization(), url.path()), format!("{origin}/idp/authorize"));
    assert_eq!(provider.authorization_param("resource"), Some(format!("{origin}/mcp")));

    options.authorization_server_metadata_url = Some(Url::parse("http://idp.example/metadata.json").unwrap());
    let error = authorize_mcp(provider.as_ref(), &options).await.unwrap_err();
    assert!(matches!(error, OAuthError::InsecureEndpoint(_)), "{error:?}");
}

#[tokio::test]
async fn exchanges_a_code_only_when_its_iss_parameter_names_the_authorization_server() {
    let codes = Arc::new(Mutex::new(Vec::new()));
    let seen = codes.clone();
    let origin = serve(move |request, _| {
        if request.method != "POST" {
            return std::future::ready(Response::status(404));
        }
        seen.lock().unwrap().push(request.form().get("code").cloned().unwrap_or_default());
        std::future::ready(Response::json(200, &json!({ "access_token": "token", "token_type": "Bearer" })))
    })
    .await;
    let exchange = |code: &str, iss: Option<&str>, iss_supported: bool, with_metadata: bool| {
        let provider = TestProvider::new("http://127.0.0.1/callback");
        {
            let mut saved = provider.saved();
            saved.client = Some(OAuthClientInformation::new("client"));
            saved.verifier = Some("verifier".into());
            let metadata = AuthorizationServerMetadata {
                authorization_response_iss_parameter_supported: Some(iss_supported),
                ..metadata(&origin)
            };
            saved.discovery = Some(OAuthDiscoveryState {
                authorization_server_url: origin.clone(),
                authorization_server_metadata: with_metadata.then_some(metadata),
                ..OAuthDiscoveryState::default()
            });
        }
        let mut options = flow(&format!("{origin}/mcp"));
        options.authorization_code = Some(code.into());
        options.iss = iss.map(str::to_string);
        async move { authorize_mcp(provider.as_ref(), &options).await }
    };
    let mismatch = exchange("other", Some("https://attacker.example"), false, true).await.unwrap_err();
    assert!(matches!(mismatch, OAuthError::IssuerMismatch { .. }), "{mismatch:?}");
    let missing = exchange("missing", None, true, true).await.unwrap_err();
    assert!(matches!(missing, OAuthError::IssuerMismatch { .. }), "{missing:?}");
    assert_eq!(exchange("matching", Some(&origin), true, true).await.unwrap(), OAuthFlowResult::Authorized);
    // Servers that do not promise the parameter may omit it.
    assert_eq!(exchange("omitted", None, false, true).await.unwrap(), OAuthFlowResult::Authorized);
    // Without metadata there is no issuer to compare with, so a server cannot skip the check by hiding it.
    let unverifiable = exchange("unverifiable", Some("https://attacker.example"), false, false).await.unwrap_err();
    assert!(matches!(unverifiable, OAuthError::IssuerMismatch { expected: None, .. }), "{unverifiable:?}");
    assert_eq!(exchange("no-metadata", None, false, false).await.unwrap(), OAuthFlowResult::Authorized);
    assert_eq!(*codes.lock().unwrap(), ["matching", "omitted", "no-metadata"]);
}

#[tokio::test]
async fn stops_when_cancelled_without_falling_back_to_a_redirect() {
    let stalled = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = stalled.clone();
    // Accepts every request and never answers.
    let origin = serve(move |request, _| {
        seen.lock().unwrap().push(request.path().to_string());
        async { Response::Hang }
    })
    .await;
    let run = |provider: Arc<TestProvider>| {
        let (stalled, origin) = (stalled.clone(), origin.clone());
        async move {
            let cancel = CancellationToken::new();
            let count = stalled.lock().unwrap().len();
            let mut options = flow(&format!("{origin}/mcp"));
            options.http = http().with_cancel(cancel.clone());
            let flow_provider = provider.clone();
            let task = tokio::spawn(async move { authorize_mcp(flow_provider.as_ref(), &options).await });
            wait_until(|| stalled.lock().unwrap().len() == count + 1).await;
            cancel.cancel();
            let error = task.await.unwrap().unwrap_err();
            assert!(matches!(error, OAuthError::Aborted), "{error:?}");
            assert!(provider.saved().authorization_url.is_none());
            stalled.lock().unwrap().last().cloned().unwrap()
        }
    };
    let discovery = run(TestProvider::new("http://127.0.0.1/callback")).await;
    assert_eq!(discovery, "/.well-known/oauth-protected-resource/mcp");
    // A failed refresh otherwise falls back to a new authorization.
    let refreshing = TestProvider::new("http://127.0.0.1/callback");
    {
        let mut saved = refreshing.saved();
        saved.client = Some(OAuthClientInformation::new("client"));
        saved.tokens = Some(tokens("a1", "r1"));
        saved.discovery = Some(OAuthDiscoveryState {
            authorization_server_url: origin.clone(),
            authorization_server_metadata: Some(metadata(&origin)),
            ..OAuthDiscoveryState::default()
        });
    }
    assert_eq!(run(refreshing).await, "/token");
}
