//! X.509 identity pool sources: the subject token is the certificate chain, and the STS
//! exchange runs over mutual TLS with the workload certificate and key.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

use super::external::option;
use crate::simple_options::provider_env_value;

const CONFIG_ENV: &str = "GOOGLE_API_CERTIFICATE_CONFIG";

async fn is_file(path: &str) -> bool {
    tokio::fs::symlink_metadata(path).await.is_ok_and(|metadata| metadata.is_file())
}

/// gcloud's `certificate_config.json` under `CLOUDSDK_CONFIG` or the gcloud config directory.
fn well_known_config(env: &BTreeMap<String, String>) -> PathBuf {
    let directory = provider_env_value("CLOUDSDK_CONFIG", env).map(PathBuf::from).unwrap_or_else(|| {
        if cfg!(windows) {
            PathBuf::from(provider_env_value("APPDATA", env).unwrap_or_default()).join("gcloud")
        } else {
            PathBuf::from(provider_env_value("HOME", env).unwrap_or_default()).join(".config").join("gcloud")
        }
    });
    directory.join("certificate_config.json")
}

/// DER bytes of every `CERTIFICATE` PEM block in `text`.
fn certificate_blocks(text: &str) -> Result<Vec<Vec<u8>>, String> {
    let (begin, end) = ("-----BEGIN CERTIFICATE-----", "-----END CERTIFICATE-----");
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(begin) {
        let after = &rest[start + begin.len()..];
        let Some(stop) = after.find(end) else { break };
        let body: String = after[..stop].split_whitespace().collect();
        blocks.push(STANDARD.decode(body).map_err(|error| error.to_string())?);
        rest = &after[stop + end.len()..];
    }
    Ok(blocks)
}

/// The leaf certificate in `bytes`, PEM or DER.
fn leaf_der(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.first() == Some(&0x30) {
        return Ok(bytes.to_vec());
    }
    let text = String::from_utf8_lossy(bytes);
    certificate_blocks(&text)?.into_iter().next().ok_or_else(|| "no certificate found".to_string())
}

fn to_pem(der: &[u8]) -> String {
    let encoded = STANDARD.encode(der);
    let lines: Vec<&str> = encoded.as_bytes().chunks(64).map(|line| std::str::from_utf8(line).unwrap_or("")).collect();
    format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", lines.join("\n"))
}

async fn config_path(certificate: &Value, env: &BTreeMap<String, String>) -> Result<String, String> {
    let use_default = option(certificate, "use_default_certificate_config").and_then(Value::as_bool) == Some(true);
    let location = option(certificate, "certificate_config_location").and_then(Value::as_str).filter(|s| !s.is_empty());
    match (use_default, location) {
        (false, None) => {
            return Err(
                "Either `useDefaultCertificateConfig` must be true or a `certificateConfigLocation` must be provided."
                    .into(),
            );
        }
        (true, Some(_)) => {
            return Err("Both `useDefaultCertificateConfig` and `certificateConfigLocation` cannot be provided.".into());
        }
        (_, Some(path)) if is_file(path).await => return Ok(path.into()),
        (_, Some(path)) => return Err(format!("Provided certificate config path is invalid: {path}")),
        (true, None) => {}
    }
    if let Some(path) = provider_env_value(CONFIG_ENV, env) {
        if is_file(&path).await {
            return Ok(path);
        }
        return Err(format!("Path from environment variable \"{CONFIG_ENV}\" is invalid: {path}"));
    }
    let well_known = well_known_config(env).display().to_string();
    if is_file(&well_known).await {
        return Ok(well_known);
    }
    Err(format!(
        "Could not find certificate configuration file. Searched override path, the \"{CONFIG_ENV}\" env var, and the gcloud path ({well_known})."
    ))
}

/// The JSON array of base64 DER certificates, leaf first, and an mTLS client for the exchange.
pub(super) async fn subject_token(
    certificate: &Value,
    env: &BTreeMap<String, String>,
) -> Result<(String, reqwest::Client), String> {
    let path = config_path(certificate, env).await?;
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|_| format!("Failed to read certificate config file at: {path}"))?;
    let config: Value = serde_json::from_str(&text)
        .map_err(|error| format!("Failed to parse certificate config from {path}: {error}"))?;
    let workload = &config["cert_configs"]["workload"];
    let (Some(cert_path), Some(key_path)) = (workload["cert_path"].as_str(), workload["key_path"].as_str()) else {
        return Err(format!(
            "Certificate config file ({path}) is missing required \"cert_path\" or \"key_path\" in the workload config."
        ));
    };
    let cert_error = |error: String| format!("Failed to read certificate file at {cert_path}: {error}");
    let cert = tokio::fs::read(cert_path).await.map_err(|error| cert_error(error.to_string()))?;
    let leaf = leaf_der(&cert).map_err(cert_error)?;
    let key_error = |error: String| format!("Failed to read private key file at {key_path}: {error}");
    let key = tokio::fs::read_to_string(key_path).await.map_err(|error| key_error(error.to_string()))?;
    if !key.contains("PRIVATE KEY-----") {
        return Err(key_error("not a PEM encoded private key".into()));
    }

    let chain = match option(certificate, "trust_chain_path").and_then(Value::as_str) {
        None => vec![leaf.clone()],
        Some(chain_path) => {
            let chain_error = |error: String| format!("Failed to process certificate chain from {chain_path}: {error}");
            let text = tokio::fs::read_to_string(chain_path).await.map_err(|error| chain_error(error.to_string()))?;
            let blocks = certificate_blocks(&text).map_err(chain_error)?;
            match blocks.iter().position(|block| *block == leaf) {
                None => std::iter::once(leaf.clone()).chain(blocks).collect(),
                Some(0) => blocks,
                Some(index) => {
                    return Err(format!(
                        "Leaf certificate exists in the trust chain but is not the first entry (found at index {index})."
                    ));
                }
            }
        }
    };
    let encoded: Vec<String> = chain.iter().map(|der| STANDARD.encode(der)).collect();
    let token = serde_json::to_string(&encoded).map_err(|error| error.to_string())?;

    let identity = reqwest::Identity::from_pem(format!("{key}\n{}", to_pem(&leaf)).as_bytes())
        .map_err(|error| format!("Cannot create mTLS Agent: {error}"))?;
    let proxy_env = Arc::new(env.clone());
    let proxy =
        reqwest::Proxy::custom(move |url| crate::http::resolve_proxy_url(url.as_str(), &proxy_env).ok().flatten());
    let client = reqwest::Client::builder()
        .identity(identity)
        .proxy(proxy)
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("Cannot create mTLS Agent: {}", crate::request::error_chain(&error)))?;
    Ok((token, client))
}
