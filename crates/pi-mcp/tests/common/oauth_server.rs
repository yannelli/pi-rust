//! MCP server protected by OAuth, with its own authorization server (discovery, DCR, PKCE, refresh).
//! Ported from coding-agent's test/suite/mcp-oauth-server.ts, with a sign-in prompt that plays the
//! browser.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use pi_mcp::LATEST_PROTOCOL_VERSION;
use pi_mcp::oauth::pkce::challenge_for;
use pi_mcp::runtime::McpSignInPrompt;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::http::{Request, Response, serve};

/// `iss` is sent as the `iss` parameter of authorization responses. `iss_parameter` advertises that
/// parameter and sends the server's issuer. `cimd` advertises Client ID Metadata Documents.
/// `redirect_path` replaces the path of the redirect URI, like a mixed-up authorization server.
#[derive(Clone, Default)]
pub struct OAuthServerOptions {
    pub iss: Option<String>,
    pub iss_parameter: bool,
    pub cimd: bool,
    pub redirect_path: Option<String>,
}

#[derive(Default)]
pub struct OAuthServerState {
    pub log: Vec<String>,
    /// Client metadata of dynamic client registrations.
    pub registrations: Vec<Value>,
    /// Query parameters of authorization requests.
    pub authorizations: Vec<HashMap<String, String>>,
    /// Parameters of token requests.
    pub token_requests: Vec<HashMap<String, String>>,
    /// Access tokens of session DELETE requests.
    pub deletes: Vec<Option<String>>,
    /// Paths that accept requests and never answer them.
    pub stall: HashSet<String>,
    pub stalled: Vec<String>,
    valid_tokens: HashSet<String>,
    refresh_tokens: HashSet<String>,
    challenges: HashMap<String, String>,
    issued: usize,
}

#[derive(Clone)]
pub struct OAuthMcpServer {
    pub url: String,
    pub origin: String,
    state: Arc<Mutex<OAuthServerState>>,
}

impl OAuthMcpServer {
    pub fn state(&self) -> MutexGuard<'_, OAuthServerState> {
        self.state.lock().unwrap()
    }

    pub fn log(&self) -> Vec<String> {
        self.state().log.clone()
    }

    /// Simulates access token expiry.
    pub fn expire_access_tokens(&self) {
        self.state().valid_tokens.clear();
    }

    pub fn authorization_param(&self, index: usize, name: &str) -> Option<String> {
        self.state().authorizations.get(index)?.get(name).cloned()
    }
}

impl OAuthServerState {
    fn issue_tokens(&mut self) -> Value {
        self.issued += 1;
        let (access, refresh) = (format!("access-{}", self.issued), format!("refresh-{}", self.issued));
        self.valid_tokens.insert(access.clone());
        self.refresh_tokens.insert(refresh.clone());
        json!({ "access_token": access, "refresh_token": refresh, "token_type": "Bearer", "expires_in": 3600 })
    }

    fn mcp(&mut self, request: &Request, origin: &str) -> Response {
        let token = request.header("authorization").map(|value| value.trim_start_matches("Bearer ").to_string());
        if request.method == "DELETE" {
            self.deletes.push(token.clone());
        }
        if request.method != "POST" {
            return Response::status(if request.method == "GET" { 405 } else { 200 });
        }
        let Some(token) = token.clone().filter(|token| self.valid_tokens.contains(token)) else {
            self.log.push(format!("401 {}", token.as_deref().unwrap_or("none")));
            let challenge = format!("Bearer resource_metadata=\"{origin}/.well-known/oauth-protected-resource/mcp\"");
            return Response::status(401).header("www-authenticate", &challenge);
        };
        let message = request.json();
        let Some(id) = message.get("id") else { return Response::status(202) };
        let result = match message["method"].as_str().unwrap_or_default() {
            "initialize" => json!({
                "protocolVersion": LATEST_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "issues", "version": "1.0.0" }
            }),
            "tools/list" => {
                json!({ "tools": [{ "name": "whoami", "inputSchema": { "type": "object", "properties": {} } }] })
            }
            "tools/call" => {
                self.log.push(format!("call {token}"));
                json!({ "content": [{ "type": "text", "text": format!("token {token}") }] })
            }
            _ => json!({}),
        };
        Response::json(200, &json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            .header("mcp-session-id", "session-1")
    }

    fn handle(&mut self, request: &Request, origin: &str, options: &OAuthServerOptions) -> Response {
        let path = request.path().to_string();
        if self.stall.contains(&path) {
            self.stalled.push(path);
            return Response::Hang;
        }
        match path.as_str() {
            "/mcp" => self.mcp(request, origin),
            "/.well-known/oauth-protected-resource/mcp" => {
                Response::json(200, &json!({ "resource": format!("{origin}/mcp"), "authorization_servers": [origin] }))
            }
            "/.well-known/oauth-authorization-server" => {
                let mut metadata = json!({
                    "issuer": origin,
                    "authorization_endpoint": format!("{origin}/authorize"),
                    "token_endpoint": format!("{origin}/token"),
                    "registration_endpoint": format!("{origin}/register"),
                    "response_types_supported": ["code"],
                    "code_challenge_methods_supported": ["S256"],
                    "token_endpoint_auth_methods_supported": ["none"]
                });
                if options.cimd {
                    metadata["client_id_metadata_document_supported"] = json!(true);
                }
                if options.iss_parameter {
                    metadata["authorization_response_iss_parameter_supported"] = json!(true);
                }
                Response::json(200, &metadata)
            }
            "/register" => {
                let mut metadata = request.json();
                self.log.push("register".into());
                self.registrations.push(metadata.clone());
                metadata["client_id"] = json!("client-1");
                Response::json(201, &metadata)
            }
            "/authorize" => {
                let query = request.query();
                let code = format!("code-{}", self.challenges.len() + 1);
                self.challenges.insert(code.clone(), query.get("code_challenge").cloned().unwrap_or_default());
                let mut redirect = Url::parse(query.get("redirect_uri").map_or("", String::as_str)).unwrap();
                if let Some(path) = &options.redirect_path {
                    redirect.set_path(path);
                }
                let state = query.get("state").cloned().unwrap_or_default();
                redirect.query_pairs_mut().append_pair("code", &code).append_pair("state", &state);
                let iss = options.iss.clone().or_else(|| options.iss_parameter.then(|| origin.to_string()));
                if let Some(iss) = iss {
                    redirect.query_pairs_mut().append_pair("iss", &iss);
                }
                self.authorizations.push(query);
                Response::status(302).header("location", redirect.as_str())
            }
            "/token" => self.token(request.form()),
            _ => Response::status(404),
        }
    }

    fn token(&mut self, params: HashMap<String, String>) -> Response {
        self.token_requests.push(params.clone());
        let param = |name: &str| params.get(name).cloned().unwrap_or_default();
        if param("grant_type") == "authorization_code" {
            let challenge = self.challenges.get(&param("code")).cloned();
            if challenge.is_none_or(|challenge| challenge != challenge_for(&param("code_verifier"))) {
                return Response::json(400, &json!({ "error": "invalid_grant" }));
            }
            self.challenges.remove(&param("code"));
            self.log.push("token code".into());
            let tokens = self.issue_tokens();
            return Response::json(200, &tokens);
        }
        if !self.refresh_tokens.remove(&param("refresh_token")) {
            return Response::json(400, &json!({ "error": "invalid_grant" }));
        }
        self.log.push("token refresh".into());
        let tokens = self.issue_tokens();
        Response::json(200, &tokens)
    }
}

pub async fn start_oauth_mcp_server(options: OAuthServerOptions) -> OAuthMcpServer {
    let state = Arc::new(Mutex::new(OAuthServerState::default()));
    let handler_state = state.clone();
    let origin = serve(move |request, origin| {
        let response = handler_state.lock().unwrap().handle(&request, &origin, &options);
        async { response }
    })
    .await;
    OAuthMcpServer { url: format!("{origin}/mcp"), origin, state }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Browser {
    /// The browser follows the authorization redirect to the loopback callback.
    Follow,
    /// The browser cannot reach the callback; the user pastes the redirect URL.
    Paste,
    /// The user never approves.
    Ignore,
}

pub struct BrowserPrompt {
    browser: Browser,
    pub opened: Mutex<Vec<Url>>,
    location: watch::Sender<Option<String>>,
}

impl BrowserPrompt {
    pub fn new(browser: Browser) -> Arc<Self> {
        Arc::new(Self { browser, opened: Mutex::default(), location: watch::channel(None).0 })
    }

    pub fn opened_param(&self, index: usize, name: &str) -> Option<String> {
        let url = self.opened.lock().unwrap().get(index)?.clone();
        url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
    }
}

#[async_trait]
impl McpSignInPrompt for BrowserPrompt {
    fn show_authorization_url(&self, url: &Url) {
        self.opened.lock().unwrap().push(url.clone());
        let url = url.clone();
        match self.browser {
            Browser::Follow => {
                tokio::spawn(reqwest::get(url));
            }
            Browser::Paste => {
                let location = self.location.clone();
                tokio::spawn(async move {
                    let client =
                        reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
                    let response = client.get(url).send().await.unwrap();
                    let value = response.headers().get("location").and_then(|value| value.to_str().ok());
                    location.send_replace(Some(value.unwrap_or_default().to_string()));
                });
            }
            Browser::Ignore => {}
        }
    }

    async fn prompt_for_redirect_url(&self, cancel: CancellationToken) -> Option<String> {
        if self.browser != Browser::Paste {
            cancel.cancelled().await;
            return None;
        }
        let mut location = self.location.subscribe();
        location.wait_for(Option::is_some).await.ok()?.clone()
    }
}
