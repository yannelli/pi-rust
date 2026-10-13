//! llama.cpp router client: model catalog, props, load, unload, and download requests
//! (upstream `extensions/llama/client.ts`). Waiting with progress lives in `wait.rs`.

use std::fmt;
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const REQUEST_TIMEOUT: Duration = Duration::from_millis(15_000);

/// Model states the router reports; other values pass through as strings.
pub mod status {
    pub const UNLOADED: &str = "unloaded";
    pub const LOADING: &str = "loading";
    pub const LOADED: &str = "loaded";
    pub const DOWNLOADING: &str = "downloading";
    pub const SLEEPING: &str = "sleeping";
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LlamaModelStatus {
    pub value: String,
    /// The `llama-server` command line of the model instance.
    pub args: Option<Vec<String>>,
    pub failed: Option<bool>,
    pub exit_code: Option<i64>,
    /// Download progress per file: `{ "<url>": { "done": n, "total": n } }`.
    pub progress: Option<Value>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LlamaArchitecture {
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LlamaModelMeta {
    pub n_ctx: Option<u64>,
    pub n_ctx_train: Option<u64>,
    pub size: Option<u64>,
    pub ftype: Option<String>,
}

/// One entry of the router's `GET /models`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LlamaModelInfo {
    pub id: String,
    pub aliases: Option<Vec<String>>,
    pub status: LlamaModelStatus,
    pub architecture: Option<LlamaArchitecture>,
    /// `preset`, `cache`, or `models_dir`.
    pub source: Option<String>,
    pub meta: Option<LlamaModelMeta>,
}

fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    let items = value?.as_array()?;
    Some(items.iter().filter_map(Value::as_str).map(String::from).collect())
}

fn positive(value: Option<&Value>) -> Option<u64> {
    value?.as_f64().filter(|number| number.is_finite() && *number >= 0.0).map(|number| number as u64)
}

impl LlamaModelInfo {
    pub fn new(id: impl Into<String>, status: &str) -> Self {
        Self {
            id: id.into(),
            status: LlamaModelStatus { value: status.into(), ..Default::default() },
            ..Default::default()
        }
    }

    /// An entry with a string `id` and `status.value`; other fields are read leniently.
    pub fn from_value(value: &Value) -> Option<Self> {
        let id = value.get("id")?.as_str()?.to_string();
        let status = value.get("status")?;
        let status = LlamaModelStatus {
            value: status.get("value")?.as_str()?.to_string(),
            args: strings(status.get("args")),
            failed: status.get("failed").and_then(Value::as_bool),
            exit_code: status.get("exit_code").and_then(Value::as_i64),
            progress: status.get("progress").filter(|progress| progress.is_object()).cloned(),
        };
        let architecture = value.get("architecture").filter(|value| value.is_object()).map(|value| LlamaArchitecture {
            input_modalities: strings(value.get("input_modalities")),
            output_modalities: strings(value.get("output_modalities")),
        });
        let meta = value.get("meta").filter(|value| value.is_object()).map(|value| LlamaModelMeta {
            n_ctx: positive(value.get("n_ctx")),
            n_ctx_train: positive(value.get("n_ctx_train")),
            size: positive(value.get("size")),
            ftype: value.get("ftype").and_then(Value::as_str).map(String::from),
        });
        let source = value.get("source").and_then(Value::as_str).map(String::from);
        Some(Self { id, aliases: strings(value.get("aliases")), status, architecture, source, meta })
    }

    pub fn is_status(&self, value: &str) -> bool {
        self.status.value == value
    }

    /// Loaded or sleeping (a slept model wakes on its next request).
    pub fn is_loaded(&self) -> bool {
        self.is_status(status::LOADED) || self.is_status(status::SLEEPING)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LlamaServerProps {
    pub models_autoload: Option<bool>,
    pub chat_template: Option<String>,
}

/// One event of `GET /models/sse`.
#[derive(Clone, Debug, PartialEq)]
pub struct LlamaModelEvent {
    pub model: String,
    pub event: String,
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LlamaProgress {
    pub message: String,
    /// Fraction done, 0 to 1.
    pub ratio: Option<f64>,
    pub detail: Option<String>,
}

impl LlamaProgress {
    pub fn message(message: &str) -> Self {
        Self { message: message.into(), ..Self::default() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LlamaError {
    Cancelled,
    /// The server could not be reached or the request timed out.
    Connection(String),
    /// The server answered with an error or an unexpected payload.
    Server(String),
}

impl LlamaError {
    pub fn is_connection_error(&self) -> bool {
        matches!(self, Self::Connection(_))
    }
}

impl fmt::Display for LlamaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Cancelled"),
            Self::Connection(message) | Self::Server(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for LlamaError {}

/// `payload.error.message` when it is a non-empty string.
pub(crate) fn error_message(payload: Option<&Value>, fallback: &str) -> String {
    payload
        .and_then(|payload| payload.get("error"))
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

/// JavaScript `toFixed`: halves round up.
pub(crate) fn to_fixed(value: f64, digits: i32) -> String {
    let scale = 10f64.powi(digits);
    format!("{:.*}", digits as usize, (value * scale).round() / scale)
}

/// `512 B`, `1.00 KiB`, `12.5 GiB`.
pub fn format_bytes(bytes: f64) -> String {
    if bytes < 1024.0 {
        return format!("{bytes} B");
    }
    let units = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes / 1024.0;
    let mut unit = units[0];
    for next in &units[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    format!("{} {unit}", to_fixed(value, if value >= 10.0 { 1 } else { 2 }))
}

/// The router URL without `/v1`, query, fragment, or trailing slash; only http and https.
pub fn normalize_llama_server_url(value: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(value.trim()).map_err(|_| format!("Invalid URL: {}", value.trim()))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("Server URL must use http or https".into());
    }
    url.set_fragment(None);
    url.set_query(None);
    let trimmed = url.path().trim_end_matches('/');
    let path = trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string();
    url.set_path(if path.is_empty() { "/" } else { &path });
    Ok(url.to_string().trim_end_matches('/').to_string())
}

/// The OpenAI-compatible base URL: the router URL plus `/v1`.
pub fn llama_inference_url(server_url: &str) -> Result<String, String> {
    Ok(format!("{}/v1", normalize_llama_server_url(server_url)?))
}

/// A llama.cpp router at a normalized URL with an optional API key.
#[derive(Clone)]
pub struct LlamaClient {
    pub(crate) server_url: String,
    pub(crate) api_key: Option<String>,
    pub(crate) http: reqwest::Client,
}

pub(crate) async fn cancellable<T>(
    cancel: &CancellationToken,
    future: impl Future<Output = Result<T, LlamaError>>,
) -> Result<T, LlamaError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(LlamaError::Cancelled),
        result = future => result,
    }
}

pub(crate) fn transport_error(error: reqwest::Error) -> LlamaError {
    LlamaError::Connection(crate::platform::http::format_network_error(&error))
}

impl LlamaClient {
    pub fn new(server_url: &str, api_key: Option<String>) -> Result<Self, String> {
        let server_url = normalize_llama_server_url(server_url)?;
        let api_key = api_key.filter(|key| !key.is_empty());
        Ok(Self { server_url, api_key, http: pi_ai::http::shared_client() })
    }

    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    pub(crate) fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => request.header("Authorization", format!("Bearer {key}")),
            None => request,
        }
    }

    async fn request(&self, url: &str, body: Option<Value>, cancel: &CancellationToken) -> Result<Value, LlamaError> {
        let mut request = match &body {
            Some(body) => self.http.post(url).header("Content-Type", "application/json").body(body.to_string()),
            None => self.http.get(url),
        };
        request = self.authorize(request);
        let exchange = async {
            let response = request.send().await.map_err(transport_error)?;
            let status = response.status();
            let payload = response.bytes().await.map_err(transport_error)?;
            let payload: Option<Value> = serde_json::from_slice(&payload).ok();
            if !status.is_success() {
                let fallback = format!("llama.cpp returned HTTP {}", status.as_u16());
                return Err(LlamaError::Server(error_message(payload.as_ref(), &fallback)));
            }
            Ok(payload.unwrap_or(Value::Null))
        };
        cancellable(cancel, async {
            tokio::time::timeout(REQUEST_TIMEOUT, exchange)
                .await
                .unwrap_or_else(|_| Err(LlamaError::Connection("llama.cpp request timeout".into())))
        })
        .await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.server_url)
    }

    /// `GET /models`, with `?reload=1` to rescan the model directories.
    pub async fn list(&self, reload: bool, cancel: &CancellationToken) -> Result<Vec<LlamaModelInfo>, LlamaError> {
        let path = if reload { "/models?reload=1" } else { "/models" };
        let payload = self.request(&self.url(path), None, cancel).await?;
        let Some(data) = payload.get("data").and_then(Value::as_array) else {
            return Err(LlamaError::Server("llama.cpp returned an invalid model catalog".into()));
        };
        data.iter()
            .map(LlamaModelInfo::from_value)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| LlamaError::Server("Server is not running in llama.cpp router mode".into()))
    }

    /// `GET /props`, for one model without loading it when `model` is given.
    pub async fn props(&self, model: Option<&str>, cancel: &CancellationToken) -> Result<LlamaServerProps, LlamaError> {
        let mut url =
            reqwest::Url::parse(&self.url("/props")).map_err(|error| LlamaError::Server(error.to_string()))?;
        if let Some(model) = model.filter(|model| !model.is_empty()) {
            url.query_pairs_mut().append_pair("model", model).append_pair("autoload", "false");
        }
        let payload = self.request(url.as_str(), None, cancel).await?;
        Ok(LlamaServerProps {
            models_autoload: payload.get("models_autoload").and_then(Value::as_bool),
            chat_template: payload.get("chat_template").and_then(Value::as_str).map(String::from),
        })
    }

    pub async fn load(&self, model: &str, cancel: &CancellationToken) -> Result<(), LlamaError> {
        self.request(&self.url("/models/load"), Some(json!({ "model": model })), cancel).await.map(drop)
    }

    pub async fn unload(&self, model: &str, cancel: &CancellationToken) -> Result<(), LlamaError> {
        self.request(&self.url("/models/unload"), Some(json!({ "model": model })), cancel).await.map(drop)
    }

    /// `POST /models`: the router downloads `owner/repository[:quant]` from Hugging Face.
    pub async fn download(&self, model: &str, cancel: &CancellationToken) -> Result<(), LlamaError> {
        self.request(&self.url("/models"), Some(json!({ "model": model })), cancel).await.map(drop)
    }
}

/// Load progress from a `status_change` event: the stage name and the fraction of all stages.
pub fn parse_load_progress(data: Option<&Value>) -> Option<LlamaProgress> {
    let progress = data?.get("progress")?.as_object()?;
    let stage = progress.get("current").or_else(|| progress.get("stage")).and_then(Value::as_str);
    let stages: Vec<&str> = progress
        .get("stages")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let stage_ratio = progress.get("value").and_then(Value::as_f64).map(|value| value.clamp(0.0, 1.0));
    let mut ratio = stage_ratio;
    if let Some(stage) = stage
        && let Some(index) = stages.iter().position(|candidate| *candidate == stage)
    {
        ratio = Some((index as f64 + stage_ratio.unwrap_or(0.0)) / stages.len() as f64);
    }
    let message = match stage {
        Some(stage) => format!("Loading {}", stage.replace('_', " ")),
        None => "Loading model".into(),
    };
    Some(LlamaProgress { message, ratio, detail: None })
}

/// Download progress summed over files, from an event (`{ progress: {...} }`) or a catalog entry.
pub fn parse_download_progress(data: Option<&Value>) -> Option<LlamaProgress> {
    let data = data?.as_object()?;
    let files = data.get("progress").and_then(Value::as_object).unwrap_or(data);
    let (mut done, mut total) = (0.0, 0.0);
    for entry in files.values() {
        if let (Some(entry_done), Some(entry_total)) =
            (entry.get("done").and_then(Value::as_f64), entry.get("total").and_then(Value::as_f64))
        {
            done += entry_done;
            total += entry_total;
        }
    }
    (total > 0.0).then(|| LlamaProgress {
        message: "Downloading model".into(),
        ratio: Some(done / total),
        detail: Some(format!("{} / {}", format_bytes(done), format_bytes(total))),
    })
}
