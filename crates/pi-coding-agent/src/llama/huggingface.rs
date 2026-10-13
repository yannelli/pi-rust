//! Hugging Face search for GGUF repositories, quantizations with sizes, gated-repository
//! status, and token lookup (upstream `extensions/llama/huggingface.ts`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::client::{LlamaError, cancellable, transport_error};

pub const DEFAULT_HUGGING_FACE_URL: &str = "https://huggingface.co";
const REQUEST_TIMEOUT: Duration = Duration::from_millis(15_000);

static QUANTIZATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[-_.])((?:UD-)?(?:IQ\d(?:_[A-Z0-9]+)+|Q\d(?:_[A-Z0-9]+)+|BF16|F16|F32|MXFP\d(?:_[A-Z0-9]+)*))$",
    )
    .expect("valid pattern")
});
static SHARD_SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-\d{5}-of-\d{5}$").expect("valid pattern"));

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HuggingFaceModel {
    pub id: String,
    pub downloads: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HuggingFaceQuantization {
    pub name: String,
    /// Total bytes over all shards, when every shard reports its size.
    pub size: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatedAccess {
    Open,
    /// Accepting the access terms grants access.
    Auto,
    /// The owner approves each request.
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HuggingFaceModelDetails {
    pub id: String,
    pub gated: GatedAccess,
    /// `Q4_K_M` first, then by size and name.
    pub quantizations: Vec<HuggingFaceQuantization>,
}

fn read_token(path: &PathBuf) -> Option<String> {
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// `HF_TOKEN`, then the token files at `$HF_TOKEN_PATH`, `$HF_HOME/token`,
/// `$XDG_CACHE_HOME/huggingface/token`, and `~/.cache/huggingface/token`.
pub fn find_hugging_face_token_with(env: &dyn Fn(&str) -> Option<String>, home: &std::path::Path) -> Option<String> {
    if let Some(token) = env("HF_TOKEN").map(|token| token.trim().to_string()).filter(|token| !token.is_empty()) {
        return Some(token);
    }
    let set = |name: &str| env(name).filter(|value| !value.is_empty());
    let candidates = [
        set("HF_TOKEN_PATH").map(PathBuf::from),
        set("HF_HOME").map(|home| PathBuf::from(home).join("token")),
        set("XDG_CACHE_HOME").map(|cache| PathBuf::from(cache).join("huggingface").join("token")),
        Some(home.join(".cache").join("huggingface").join("token")),
    ];
    let mut seen = Vec::new();
    for path in candidates.into_iter().flatten() {
        if seen.contains(&path) {
            continue;
        }
        if let Some(token) = read_token(&path) {
            return Some(token);
        }
        seen.push(path);
    }
    None
}

/// [`find_hugging_face_token_with`] for this process.
pub fn find_hugging_face_token() -> Option<String> {
    find_hugging_face_token_with(&|name| std::env::var(name).ok(), &crate::config::home_dir())
}

/// The `t=<seconds>` value of an IETF `RateLimit` header.
fn rate_limit_delay(value: Option<&str>) -> Option<u64> {
    static DELAY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|;)t=(\d+)").expect("valid pattern"));
    DELAY.captures(value?).and_then(|captures| captures[1].parse().ok())
}

fn encode_segment(segment: &str) -> String {
    let mut encoded = String::new();
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

pub struct HuggingFaceClient {
    token: Option<String>,
    base_url: String,
    http: reqwest::Client,
}

impl HuggingFaceClient {
    pub fn new(token: Option<String>, base_url: Option<&str>) -> Self {
        let base_url = base_url.unwrap_or(DEFAULT_HUGGING_FACE_URL).trim_end_matches('/').to_string();
        Self { token: token.filter(|token| !token.is_empty()), base_url, http: pi_ai::http::shared_client() }
    }

    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    async fn request(&self, url: &str, cancel: &CancellationToken) -> Result<Value, LlamaError> {
        let mut request = self.http.get(url);
        if let Some(token) = &self.token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let exchange = async {
            let response = request.send().await.map_err(transport_error)?;
            let status = response.status().as_u16();
            let header =
                |name: &str| response.headers().get(name).and_then(|value| value.to_str().ok()).map(String::from);
            let (retry_after, rate_limit) = (header("retry-after"), header("ratelimit"));
            let payload: Option<Value> =
                response.bytes().await.ok().and_then(|body| serde_json::from_slice(&body).ok());
            if (200..300).contains(&status) {
                return Ok(payload.unwrap_or(Value::Null));
            }
            if status == 429 {
                let delay = retry_after
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .filter(|delay| *delay > 0)
                    .or_else(|| rate_limit_delay(rate_limit.as_deref()));
                return Err(LlamaError::Server(match delay {
                    Some(delay) => format!("Hugging Face rate limit reached; retry in {delay}s"),
                    None => "Hugging Face rate limit reached".into(),
                }));
            }
            let error = payload.as_ref().and_then(|payload| payload.get("error")).and_then(Value::as_str);
            let message = error.filter(|error| !error.is_empty()).map(String::from);
            Err(LlamaError::Server(message.unwrap_or_else(|| format!("Hugging Face returned HTTP {status}"))))
        };
        cancellable(cancel, async {
            tokio::time::timeout(REQUEST_TIMEOUT, exchange)
                .await
                .unwrap_or_else(|_| Err(LlamaError::Connection("Hugging Face request timeout".into())))
        })
        .await
    }

    /// The 20 most downloaded GGUF repositories matching `query`.
    pub async fn search(&self, query: &str, cancel: &CancellationToken) -> Result<Vec<HuggingFaceModel>, LlamaError> {
        let mut url = reqwest::Url::parse(&format!("{}/api/models", self.base_url))
            .map_err(|error| LlamaError::Server(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair("search", query)
            .append_pair("filter", "gguf")
            .append_pair("sort", "downloads")
            .append_pair("direction", "-1")
            .append_pair("limit", "20");
        let payload = self.request(url.as_str(), cancel).await?;
        let Some(items) = payload.as_array() else {
            return Err(LlamaError::Server("Hugging Face returned invalid search results".into()));
        };
        Ok(items
            .iter()
            .filter_map(|item| {
                let id = item.get("id")?.as_str()?.to_string();
                let downloads = item.get("downloads").and_then(Value::as_f64).map(|count| count as u64).unwrap_or(0);
                Some(HuggingFaceModel { id, downloads })
            })
            .collect())
    }

    /// Quantizations of a repository's GGUF files (shards summed, `mmproj` files skipped) and
    /// whether access is gated.
    pub async fn details(&self, id: &str, cancel: &CancellationToken) -> Result<HuggingFaceModelDetails, LlamaError> {
        let encoded: Vec<String> = id.split('/').map(encode_segment).collect();
        let url = format!("{}/api/models/{}?blobs=true", self.base_url, encoded.join("/"));
        let payload = self.request(&url, cancel).await?;
        if !payload.is_object() {
            return Err(LlamaError::Server("Hugging Face returned invalid model details".into()));
        }
        let mut sizes: BTreeMap<String, (u64, bool)> = BTreeMap::new();
        for file in payload.get("siblings").and_then(Value::as_array).into_iter().flatten() {
            let Some(name) = file.get("rfilename").and_then(Value::as_str) else { continue };
            if !name.to_ascii_lowercase().ends_with(".gguf") {
                continue;
            }
            let filename = name.rsplit('/').next().unwrap_or(name);
            if filename.to_ascii_lowercase().starts_with("mmproj") {
                continue;
            }
            let stem = SHARD_SUFFIX.replace(&filename[..filename.len() - 5], "").into_owned();
            let Some(quantization) = QUANTIZATION.captures(&stem).map(|captures| captures[1].to_ascii_uppercase())
            else {
                continue;
            };
            let current = sizes.entry(quantization).or_insert((0, true));
            match file.get("size").and_then(Value::as_f64) {
                Some(size) => current.0 += size as u64,
                None => current.1 = false,
            }
        }
        let mut quantizations: Vec<HuggingFaceQuantization> = sizes
            .into_iter()
            .map(|(name, (total, complete))| HuggingFaceQuantization { name, size: complete.then_some(total) })
            .collect();
        quantizations.sort_by(|left, right| {
            let recommended = (right.name == "Q4_K_M").cmp(&(left.name == "Q4_K_M"));
            let size = |entry: &HuggingFaceQuantization| entry.size.unwrap_or(u64::MAX);
            recommended.then(size(left).cmp(&size(right))).then_with(|| left.name.cmp(&right.name))
        });
        let gated = match payload.get("gated").and_then(Value::as_str) {
            Some("auto") => GatedAccess::Auto,
            Some("manual") => GatedAccess::Manual,
            _ => GatedAccess::Open,
        };
        let id = payload.get("id").and_then(Value::as_str).unwrap_or(id).to_string();
        Ok(HuggingFaceModelDetails { id, gated, quantizations })
    }
}
