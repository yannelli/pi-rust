//! Sign-in to OAuth MCP servers: the authorization code flow with PKCE, a Client ID Metadata
//! Document or dynamic client registration, and a loopback callback or a pasted redirect URL.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::auth::OAUTH_REQUEST_TIMEOUT;
use crate::error::OAuthError;
use crate::oauth::pkce::{base64_url, sha256};
use crate::oauth::{
    AuthorizationServerMetadata, CallbackPage, CallbackServer, CallbackServerFactory, CallbackServerOptions,
    ClientMetadataDocument, McpOAuthProvider, McpOAuthProviderOptions, McpOAuthStateStore, OAuthChallenge,
    OAuthClientInformation, OAuthClientMetadata, OAuthFlowOptions, OAuthFlowResult, OAuthHttp, authorize_mcp,
    step_up_scope,
};

pub const APP_NAME: &str = "pi";
const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PATH: &str = "/callback";
/// Where pi.dev serves pi's Client ID Metadata Documents.
const CLIENT_METADATA_BASE_URL: &str = "https://pi.dev/oauth";

/// OAuth settings of a server, from its `oauth` config with the client secret resolved.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct McpOAuthSettings {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub callback_port: Option<u16>,
    pub callback_url: Option<String>,
    /// Scopes to request, separated by spaces.
    pub scope: Option<String>,
    /// `client_name` for dynamic client registration. Default: `pi`.
    pub client_name: Option<String>,
    /// `dcr` (default) or `cimd`.
    pub client_registration: Option<String>,
    pub auth_server_metadata_url: Option<Url>,
}

impl McpOAuthSettings {
    fn cimd(&self) -> bool {
        self.client_registration.as_deref() == Some("cimd")
    }
}

/// Where the callback server listens and the redirect URI it serves.
pub(crate) struct CallbackSettings {
    pub host: String,
    pub redirect_host: String,
    pub port: Option<u16>,
    pub path: String,
    /// The exact redirect URI, when the port is fixed.
    pub fixed_redirect_url: Option<String>,
}

pub(crate) fn callback_settings(settings: &McpOAuthSettings) -> CallbackSettings {
    let configured = settings.callback_url.as_deref().and_then(|url| Url::parse(url).ok());
    let mut url = configured
        .clone()
        .unwrap_or_else(|| Url::parse(&format!("http://{CALLBACK_HOST}{CALLBACK_PATH}")).expect("valid URL"));
    let address = url.host_str().unwrap_or(CALLBACK_HOST).trim_start_matches('[').trim_end_matches(']').to_string();
    let port = url.port().or(settings.callback_port);
    // A configured URI with a port is sent exactly as written, since servers compare it as a string.
    let fixed_redirect_url = match (url.port(), port) {
        (Some(_), _) => settings.callback_url.clone(),
        (None, Some(port)) => {
            let _ = url.set_port(Some(port));
            Some(url.to_string())
        }
        (None, None) => None,
    };
    CallbackSettings {
        // `localhost` is served on 127.0.0.1; browsers fall back to it when ::1 refuses.
        host: if address == "localhost" { CALLBACK_HOST.into() } else { address.clone() },
        redirect_host: address,
        port,
        path: url.path().to_string(),
        fixed_redirect_url,
    }
}

/// Scopes of all lists, each once.
pub(crate) fn merge_scopes(scopes: &[Option<&str>]) -> Option<String> {
    let mut merged: Vec<&str> = Vec::new();
    for scope in scopes.iter().flatten().flat_map(|scope| scope.split_whitespace()) {
        if !merged.contains(&scope) {
            merged.push(scope);
        }
    }
    (!merged.is_empty()).then(|| merged.join(" "))
}

pub(crate) fn registered_redirect_urls(client: Option<&OAuthClientInformation>) -> Vec<String> {
    client.and_then(|client| client.redirect_uris.clone()).unwrap_or_default()
}

/// 12 characters identifying an MCP server URL in callback paths, computed like Codex does.
pub fn callback_id(server_url: &Url) -> String {
    let mut url = server_url.clone();
    url.set_fragment(None);
    base64_url(&sha256(url.as_str().as_bytes())[..9])
}

/// pi's Client ID Metadata Document. Without the `iss` parameter (RFC 9207), the document and redirect
/// URI are specific to the MCP server, so responses of different authorization servers stay apart.
fn client_metadata_document(
    server_url: &Url,
    redirect_url: &str,
    metadata: Option<&AuthorizationServerMetadata>,
) -> Result<Option<ClientMetadataDocument>, OAuthError> {
    let supported = metadata.is_some_and(|metadata| {
        metadata.client_id_metadata_document_supported == Some(true)
            && metadata.token_endpoint_auth_methods_supported.iter().flatten().any(|method| method == "none")
    });
    let Some(metadata) = metadata.filter(|_| supported) else {
        return Err(OAuthError::other(
            "The authorization server does not support Client ID Metadata Documents for public clients; remove oauth.clientRegistration \"cimd\"",
        ));
    };
    if metadata.authorization_response_iss_parameter_supported == Some(true) {
        let url = format!("{CLIENT_METADATA_BASE_URL}/client.json");
        return Ok(Some(ClientMetadataDocument { url, redirect_url: redirect_url.into() }));
    }
    let id = callback_id(server_url);
    let mut redirect = Url::parse(redirect_url).map_err(|_| OAuthError::other("Invalid redirect URL"))?;
    redirect.set_path(&format!("{CALLBACK_PATH}/{id}"));
    let url = format!("{CLIENT_METADATA_BASE_URL}/{id}/client.json");
    Ok(Some(ClientMetadataDocument { url, redirect_url: redirect.to_string() }))
}

pub(crate) fn create_provider(
    server_url: &Url,
    store: Arc<dyn McpOAuthStateStore>,
    settings: &McpOAuthSettings,
    redirect_url: String,
    on_redirect: Arc<dyn Fn(&Url) + Send + Sync>,
) -> McpOAuthProvider {
    let client_name = settings.client_name.clone().unwrap_or_else(|| APP_NAME.into());
    let document = settings.cimd().then(|| {
        let (server_url, redirect_url) = (server_url.clone(), redirect_url.clone());
        Arc::new(move |metadata: Option<&AuthorizationServerMetadata>| {
            client_metadata_document(&server_url, &redirect_url, metadata)
        }) as crate::oauth::provider::ClientMetadataDocumentFn
    });
    McpOAuthProvider::new(McpOAuthProviderOptions {
        server_url: server_url.clone(),
        redirect_url,
        client_metadata: OAuthClientMetadata { client_name: Some(client_name), ..OAuthClientMetadata::default() },
        client_metadata_document: document,
        client_id: settings.client_id.clone(),
        client_secret: settings.client_secret.clone(),
        store,
        on_redirect,
    })
}

#[async_trait]
pub trait McpSignInPrompt: Send + Sync {
    /// Show the authorization URL for signing in to `server` and open it in a browser.
    fn show_authorization_url(&self, server: &str, url: &Url);
    /// Ask for the redirect URL from the browser address bar, for when the browser cannot reach the
    /// callback (for example over SSH). Cancelled once the callback arrives. `None` or "" cancels.
    async fn prompt_for_redirect_url(&self, server: &str, cancel: CancellationToken) -> Option<String>;
}

fn response_from_redirect_url(
    input: &str,
    state: &str,
    redirect_url: &Url,
) -> Result<(String, Option<String>), OAuthError> {
    let url = Url::parse(input.trim())
        .map_err(|_| OAuthError::other("Expected the full redirect URL from the browser address bar"))?;
    // A server-specific redirect URI tells authorization servers apart, so it must match exactly.
    if url.origin() != redirect_url.origin() || url.path() != redirect_url.path() {
        return Err(OAuthError::other("The redirect URL does not match this sign-in's redirect URI"));
    }
    let param = |name: &str| url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned());
    if let Some(error) = param("error") {
        return Err(OAuthError::other(param("error_description").unwrap_or(error)));
    }
    if param("state").as_deref() != Some(state) {
        return Err(OAuthError::other("The redirect URL belongs to a different sign-in"));
    }
    let code = param("code").filter(|code| !code.is_empty());
    let code = code.ok_or_else(|| OAuthError::other("The redirect URL does not contain an authorization code"))?;
    Ok((code, param("iss")))
}

/// Wait for the browser callback or a pasted redirect URL, whichever comes first.
async fn wait_for_authorization_response(
    callback: &dyn CallbackServer,
    state: &str,
    redirect_url: &Url,
    prompt: (&dyn McpSignInPrompt, &str),
    cancel: &CancellationToken,
) -> Result<(String, Option<String>), OAuthError> {
    let (prompt, server) = prompt;
    let prompt_cancel = cancel.child_token();
    let from_browser = callback.wait_for_callback(state, Some(redirect_url.path()))?;
    let from_user = async {
        let input = prompt.prompt_for_redirect_url(server, prompt_cancel.clone()).await;
        match input.filter(|input| !input.trim().is_empty()) {
            Some(input) => response_from_redirect_url(&input, state, redirect_url),
            None => Err(OAuthError::Cancelled),
        }
    };
    let result = tokio::select! {
        response = from_browser => response.map(|callback| (callback.code, callback.iss)),
        response = from_user => response,
        _ = cancel.cancelled() => Err(OAuthError::Cancelled),
    };
    prompt_cancel.cancel();
    result
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The browser page after the redirect.
pub fn sign_in_page(page: &CallbackPage) -> String {
    let (title, body) = match page {
        CallbackPage::Ok => ("Signed in", "Signed in to the MCP server. You may now close this page.".to_string()),
        CallbackPage::Failed { message, details } => {
            let details =
                details.as_deref().map(|details| format!("<pre>{}</pre>", escape_html(details))).unwrap_or_default();
            ("Sign-in failed", format!("{}{details}", escape_html(message)))
        }
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head><body><h1>{title}</h1><p>{body}</p></body></html>"
    )
}

pub struct SignInOptions {
    /// Name of the server in `mcp.json`, passed to the prompt.
    pub server_name: String,
    pub server_url: Url,
    pub store: Arc<dyn McpOAuthStateStore>,
    pub settings: McpOAuthSettings,
    pub challenge: Option<OAuthChallenge>,
    pub prompt: Arc<dyn McpSignInPrompt>,
    /// Stops the sign-in at any step with `OAuthError::Cancelled`.
    pub cancel: CancellationToken,
    pub callback_factory: Arc<dyn CallbackServerFactory>,
    pub client: reqwest::Client,
}

async fn listen_for_callback(
    factory: &dyn CallbackServerFactory,
    settings: &CallbackSettings,
    extra_paths: Vec<String>,
    port: Option<u16>,
    required: bool,
) -> Result<Arc<dyn CallbackServer>, OAuthError> {
    let options = CallbackServerOptions {
        host: settings.host.clone(),
        redirect_host: Some(settings.redirect_host.clone()),
        port: port.unwrap_or(0),
        path: settings.path.clone(),
        extra_paths,
        render_page: Some(Arc::new(sign_in_page)),
        ..CallbackServerOptions::default()
    };
    match factory.listen(options.clone()).await {
        Err(error) if !required && port.is_some() => {
            let _ = error;
            factory.listen(CallbackServerOptions { port: 0, ..options }).await
        }
        result => result,
    }
}

/// Sign in to an MCP server: refresh the stored tokens when possible, else run the browser flow.
pub async fn sign_in_mcp_server(options: SignInOptions) -> Result<(), OAuthError> {
    let cancel = options.cancel.clone();
    if cancel.is_cancelled() {
        return Err(OAuthError::Cancelled);
    }
    let settings = &options.settings;
    let stored = options.store.load().await?;
    let cb = callback_settings(settings);
    // Reuse the port of the registered redirect URI so the registered client stays valid.
    let registered = registered_redirect_urls(stored.as_ref().and_then(|state| state.client_information.as_ref()));
    let registered_port = registered.first().and_then(|url| Url::parse(url).ok()).and_then(|url| url.port());
    let extra_paths = if settings.cimd() {
        vec![format!("{CALLBACK_PATH}/{}", callback_id(&options.server_url))]
    } else {
        Vec::new()
    };
    let callback = listen_for_callback(
        options.callback_factory.as_ref(),
        &cb,
        extra_paths,
        cb.port.or(registered_port),
        cb.port.is_some(),
    )
    .await?;
    let redirect_url = cb.fixed_redirect_url.clone().unwrap_or_else(|| callback.redirect_url());
    let result = run_sign_in(&options, stored, callback.as_ref(), redirect_url).await;
    callback.close().await;
    // Aborted requests fail with their own errors; report them as the cancellation they are.
    match result {
        Err(_) if cancel.is_cancelled() => Err(OAuthError::Cancelled),
        result => result,
    }
}

async fn run_sign_in(
    options: &SignInOptions,
    stored: Option<crate::oauth::McpOAuthState>,
    callback: &dyn CallbackServer,
    redirect_url: String,
) -> Result<(), OAuthError> {
    let settings = &options.settings;
    let step_up =
        options.challenge.as_ref().and_then(|challenge| challenge.error.as_deref()) == Some("insufficient_scope");
    if let Some(stored) = &stored {
        let mut next = stored.clone();
        // Every sign-in gets a fresh `state` parameter.
        next.oauth_state = None;
        // A registered client cannot use another redirect URI, and its tokens belong to it.
        let keep_client = settings.client_id.is_some()
            || if settings.cimd() {
                stored.client_information.is_none()
            } else {
                registered_redirect_urls(stored.client_information.as_ref()).contains(&redirect_url)
            };
        if !keep_client {
            next.client_information = None;
            next.tokens = None;
            next.tokens_expire_at = None;
        }
        options.store.save(next).await?;
    }
    let authorization_url: Arc<Mutex<Option<Url>>> = Arc::default();
    let shown = authorization_url.clone();
    let on_redirect =
        Arc::new(move |url: &Url| *shown.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(url.clone()));
    let provider =
        create_provider(&options.server_url, options.store.clone(), settings, redirect_url.clone(), on_redirect);
    let http =
        OAuthHttp::new(options.client.clone()).with_timeout(OAUTH_REQUEST_TIMEOUT).with_cancel(options.cancel.clone());
    let mut flow = OAuthFlowOptions::new(options.server_url.clone(), http);
    let challenge = options.challenge.as_ref();
    flow.resource_metadata_url = challenge.and_then(|challenge| challenge.resource_metadata_url.clone());
    flow.authorization_server_metadata_url = settings.auth_server_metadata_url.clone();
    let challenged = challenge.and_then(|challenge| challenge.scope.as_deref());
    // A server asking for more scope gets it on top of the configured scope and the scope granted so far.
    let extra = if step_up {
        let granted =
            stored.as_ref().and_then(|state| state.tokens.as_ref()).and_then(|tokens| tokens.scope.as_deref());
        step_up_scope(granted, challenged)
    } else {
        challenged.map(str::to_string)
    };
    flow.scope = merge_scopes(&[settings.scope.as_deref(), extra.as_deref()]);
    let refresh_flow = OAuthFlowOptions { skip_refresh: step_up, ..flow.clone() };
    if authorize_mcp(&provider, &refresh_flow).await? == OAuthFlowResult::Authorized {
        return Ok(());
    }
    let url = authorization_url.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
    let url = url.ok_or_else(|| OAuthError::other("OAuth flow did not produce an authorization URL"))?;
    let state = provider.oauth_state().await?;
    // The flow picks the redirect URI, which may be specific to the MCP server.
    let redirect = url.query_pairs().find(|(key, _)| key == "redirect_uri").map(|(_, value)| value.into_owned());
    let redirect = Url::parse(redirect.as_deref().unwrap_or(&redirect_url))
        .map_err(|_| OAuthError::other("Invalid redirect URI"))?;
    options.prompt.show_authorization_url(&options.server_name, &url);
    let prompt = (options.prompt.as_ref(), options.server_name.as_str());
    let (code, iss) = wait_for_authorization_response(callback, &state, &redirect, prompt, &options.cancel).await?;
    let exchange = OAuthFlowOptions { authorization_code: Some(code), iss, ..flow };
    authorize_mcp(&provider, &exchange).await.map(|_| ())
}
