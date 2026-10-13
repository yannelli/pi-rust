//! Byte streams for the Codex WebSocket: direct TCP, or a `CONNECT` tunnel through the
//! HTTP(S) proxy that applies to the target, like the proxy agent upstream hands its socket.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use base64::Engine;
use reqwest::Url;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::error::TlsError;

use super::events::CodexError;
use crate::http::resolve_proxy_url;

pub(super) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// A stream to the WebSocket host, possibly through a proxy tunnel.
pub(super) type Tunnel = Box<dyn Io>;

const MAX_PROXY_RESPONSE_HEAD: usize = 64 * 1024;

/// rustls with ring and the webpki roots, for the target and for HTTPS proxies.
pub(super) fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(config)
    });
    CONFIG.clone()
}

fn transport(message: impl Into<String>) -> CodexError {
    CodexError::Transport(message.into())
}

fn io_error(error: std::io::Error) -> CodexError {
    transport(WsError::Io(error).to_string())
}

/// The proxy for a `ws:`/`wss:` URL, resolved as for the same URL over `http:`/`https:`.
pub(super) fn proxy_for(url: &Url, env: &BTreeMap<String, String>) -> Result<Option<Url>, CodexError> {
    let mut target = url.clone();
    let scheme = match url.scheme() {
        "wss" => "https",
        "ws" => "http",
        other => other,
    };
    if target.set_scheme(scheme).is_err() {
        return Ok(None);
    }
    resolve_proxy_url(target.as_str(), env).map_err(transport)
}

fn host_port(url: &Url) -> Result<(&str, u16), CodexError> {
    let host = url.host_str().ok_or_else(|| transport(format!("Invalid URL: {url}")))?;
    Ok((host, url.port_or_known_default().unwrap_or(80)))
}

fn bare(host: &str) -> &str {
    host.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(host)
}

async fn tcp(host: &str, port: u16) -> Result<TcpStream, CodexError> {
    TcpStream::connect((bare(host), port)).await.map_err(io_error)
}

/// Open a stream to the host of `url`, through the proxy that applies to it, if any.
pub(super) async fn open(url: &Url, env: &BTreeMap<String, String>) -> Result<Tunnel, CodexError> {
    let (host, port) = host_port(url)?;
    let Some(proxy) = proxy_for(url, env)? else { return Ok(Box::new(tcp(host, port).await?)) };
    let (proxy_host, proxy_port) = host_port(&proxy)?;
    let socket = tcp(proxy_host, proxy_port).await?;
    let mut stream: Tunnel = if proxy.scheme() == "https" {
        let name = ServerName::try_from(bare(proxy_host).to_string())
            .map_err(|_| transport(WsError::Tls(TlsError::InvalidDnsName).to_string()))?;
        let connector = tokio_rustls::TlsConnector::from(tls_config());
        Box::new(connector.connect(name, socket).await.map_err(io_error)?)
    } else {
        Box::new(socket)
    };
    let request = connect_request(host, port, url.scheme() == "wss", &proxy);
    stream.write_all(request.as_bytes()).await.map_err(io_error)?;
    stream.flush().await.map_err(io_error)?;
    read_connect_response(&mut stream).await?;
    Ok(stream)
}

/// The `CONNECT` request; `Host` carries the port only when it is not the default.
pub(super) fn connect_request(host: &str, port: u16, secure: bool, proxy: &Url) -> String {
    let default_port = if secure { 443 } else { 80 };
    let host_header = if port == default_port { host.to_string() } else { format!("{host}:{port}") };
    let mut request = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host_header}\r\n");
    if !proxy.username().is_empty() || proxy.password().is_some() {
        let user = percent_decode(proxy.username());
        let password = percent_decode(proxy.password().unwrap_or_default());
        let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        request.push_str(&format!("Proxy-Authorization: Basic {credentials}\r\n"));
    }
    request.push_str("\r\n");
    request
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes.get(index + 1..index + 3).and_then(|pair| std::str::from_utf8(pair).ok());
        match hex.filter(|_| bytes[index] == b'%').and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Read the proxy's response head one byte at a time, so no tunneled bytes are consumed.
async fn read_connect_response(stream: &mut Tunnel) -> Result<(), CodexError> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_PROXY_RESPONSE_HEAD {
            return Err(transport("Proxy response headers are too large"));
        }
        if stream.read(&mut byte).await.map_err(io_error)? == 0 {
            return Err(transport("Proxy connection ended before receiving CONNECT response"));
        }
        head.push(byte[0]);
    }
    let status_line = String::from_utf8_lossy(head.split(|byte| *byte == b'\n').next().unwrap_or_default());
    let status = status_line.split_whitespace().nth(1).and_then(|code| code.parse::<u16>().ok());
    match status {
        Some(200..=299) => Ok(()),
        Some(code) => Err(transport(format!("Unexpected server response: {code}"))),
        None => Err(transport(format!("Invalid proxy response: {}", status_line.trim()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_requests_carry_the_target_and_decoded_credentials() {
        let proxy = Url::parse("http://us%40er:p%3Ass@proxy:8080").unwrap();
        let request = connect_request("chatgpt.com", 443, true, &proxy);
        let credentials = base64::engine::general_purpose::STANDARD.encode("us@er:p:ss");
        assert_eq!(
            request,
            format!(
                "CONNECT chatgpt.com:443 HTTP/1.1\r\nHost: chatgpt.com\r\nProxy-Authorization: Basic {credentials}\r\n\r\n"
            )
        );
        let plain = connect_request("[::1]", 8080, false, &Url::parse("http://proxy:1").unwrap());
        assert_eq!(plain, "CONNECT [::1]:8080 HTTP/1.1\r\nHost: [::1]:8080\r\n\r\n");
        assert_eq!(percent_decode("a%2Fb%zz%4"), "a/b%zz%4");
    }

    #[test]
    fn websocket_schemes_resolve_like_http_schemes() {
        let env: BTreeMap<String, String> =
            [("HTTPS_PROXY", "http://secure:1"), ("HTTP_PROXY", "plain:2"), ("NO_PROXY", "local.test")]
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect();
        let proxy = |url: &str| proxy_for(&Url::parse(url).unwrap(), &env).unwrap().map(|url| url.to_string());
        assert_eq!(proxy("wss://chatgpt.com/backend-api/codex/responses").as_deref(), Some("http://secure:1/"));
        assert_eq!(proxy("ws://chatgpt.com:8080/x").as_deref(), Some("http://plain:2/"));
        assert_eq!(proxy("wss://api.local.test/x"), None);
        let socks: BTreeMap<String, String> = [("HTTPS_PROXY".to_string(), "socks5://proxy:1080".to_string())].into();
        let error = proxy_for(&Url::parse("wss://chatgpt.com").unwrap(), &socks).unwrap_err();
        assert!(error.message().starts_with(crate::http::UNSUPPORTED_PROXY_PROTOCOL_MESSAGE));
    }
}
