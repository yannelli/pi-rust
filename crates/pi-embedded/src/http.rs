//! HTTP network adapter over reqwest with rustls (upstream Node `FetchNetworkAdapter`).

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::host::{NetworkAdapter, NetworkResponse};
use crate::types::NetworkRequest;

/// Incremental UTF-8 decoding that keeps split multibyte sequences for the next chunk and
/// replaces invalid bytes with U+FFFD, like `TextDecoder` in streaming mode.
#[derive(Default)]
pub(crate) struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut output = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    output.push_str(text);
                    self.pending.clear();
                    return output;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    output.push_str(std::str::from_utf8(&self.pending[..valid]).unwrap_or_default());
                    match error.error_len() {
                        Some(invalid) => {
                            output.push('\u{FFFD}');
                            self.pending.drain(..valid + invalid);
                        }
                        None => {
                            self.pending.drain(..valid);
                            return output;
                        }
                    }
                }
            }
        }
    }

    pub fn finish(&mut self) -> String {
        let rest = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        rest
    }
}

/// Streams provider responses with reqwest. Cancellation aborts connection setup and ends a
/// body read with an error; dropping the body closes the connection.
#[derive(Clone, Default)]
pub struct HttpNetworkAdapter {
    client: reqwest::Client,
}

impl HttpNetworkAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

/// Forward the body through a task so cancellation closes the connection even while the
/// consumer is not reading; dropping the returned stream closes it as well.
fn body_stream(response: reqwest::Response, cancel: CancellationToken) -> BoxStream<'static, anyhow::Result<String>> {
    let (sender, receiver) = tokio::sync::mpsc::channel::<anyhow::Result<String>>(8);
    tokio::spawn(async move {
        let mut bytes = response.bytes_stream();
        let mut decoder = Utf8Decoder::default();
        loop {
            let next = tokio::select! {
                next = bytes.next() => next,
                _ = cancel.cancelled() => {
                    drop(bytes);
                    let _ = sender.send(Err(anyhow::anyhow!("Request cancelled"))).await;
                    return;
                }
                _ = sender.closed() => return,
            };
            let (item, last) = match next {
                Some(Ok(chunk)) => (Ok(decoder.push(&chunk)), false),
                Some(Err(error)) => (Err(error.into()), true),
                None => (Ok(decoder.finish()), true),
            };
            let empty = matches!(&item, Ok(text) if text.is_empty());
            if (!empty && sender.send(item).await.is_err()) || last {
                return;
            }
        }
    });
    futures::stream::unfold(receiver, |mut receiver| async move { receiver.recv().await.map(|item| (item, receiver)) })
        .boxed()
}

#[async_trait]
impl NetworkAdapter for HttpNetworkAdapter {
    async fn request(&self, request: NetworkRequest, cancel: CancellationToken) -> anyhow::Result<NetworkResponse> {
        if cancel.is_cancelled() {
            anyhow::bail!("Request cancelled");
        }
        let mut builder = self.client.post(&request.url).body(request.body);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let response = tokio::select! {
            response = builder.send() => response?,
            _ = cancel.cancelled() => anyhow::bail!("Request cancelled"),
        };
        let status = response.status().as_u16();
        Ok(NetworkResponse { status, body: body_stream(response, cancel) })
    }
}
