//! Waiting for loads, unloads, and downloads: the router's `/models/sse` events give progress
//! while catalog polling stays authoritative (upstream `client.ts` `loadAndWait`,
//! `downloadAndWait`, `unloadAndWait`, and `watch`).

use std::sync::Mutex;
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::client::{
    LlamaClient, LlamaError, LlamaModelEvent, LlamaModelInfo, LlamaProgress, cancellable, error_message,
    parse_download_progress, parse_load_progress, status, transport_error,
};

async fn sleep(duration: Duration, cancel: &CancellationToken) -> Result<(), LlamaError> {
    cancellable(cancel, async {
        tokio::time::sleep(duration).await;
        Ok(())
    })
    .await
}

fn parse_frame(frame: &str) -> Option<LlamaModelEvent> {
    let data: Vec<&str> =
        frame.split('\n').filter_map(|line| line.strip_prefix("data:")).map(str::trim_start).collect();
    if data.is_empty() {
        return None;
    }
    let event: Value = serde_json::from_str(&data.join("\n")).ok()?;
    Some(LlamaModelEvent {
        model: event.get("model")?.as_str()?.to_string(),
        event: event.get("event")?.as_str()?.to_string(),
        data: event.get("data").cloned(),
    })
}

/// Run `body` while watching events; the watch ends with `body`.
async fn with_watch<T>(
    client: &LlamaClient,
    cancel: &CancellationToken,
    on_event: impl FnMut(LlamaModelEvent) + Send,
    body: impl Future<Output = Result<T, LlamaError>>,
) -> Result<T, LlamaError> {
    let watcher = cancel.child_token();
    let watch = async {
        let _ = client.watch(on_event, &watcher).await;
        std::future::pending::<()>().await;
    };
    let result = tokio::select! {
        result = body => result,
        _ = watch => unreachable!("the watch future never completes"),
    };
    watcher.cancel();
    result
}

impl LlamaClient {
    /// Stream `/models/sse` events to `on_event` until the stream ends or `cancel` fires.
    /// Malformed events are skipped.
    pub async fn watch(
        &self,
        mut on_event: impl FnMut(LlamaModelEvent) + Send,
        cancel: &CancellationToken,
    ) -> Result<(), LlamaError> {
        let request = self.authorize(self.http.get(format!("{}/models/sse", self.server_url)));
        cancellable(cancel, async {
            let response = request.send().await.map_err(transport_error)?;
            if !response.status().is_success() {
                return Err(LlamaError::Server(format!("llama.cpp SSE returned HTTP {}", response.status().as_u16())));
            }
            let mut body = response.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();
            while let Some(chunk) = body.next().await {
                buffer.extend(chunk.map_err(transport_error)?.iter().filter(|byte| **byte != b'\r'));
                while let Some(boundary) = buffer.windows(2).position(|pair| pair == b"\n\n") {
                    let frame: Vec<u8> = buffer.drain(..boundary + 2).collect();
                    if let Some(event) = parse_frame(&String::from_utf8_lossy(&frame[..boundary])) {
                        on_event(event);
                    }
                }
            }
            Ok(())
        })
        .await
    }

    /// Unload `model` and poll until the router reports it unloaded or gone.
    pub async fn unload_and_wait(&self, model: &str, cancel: &CancellationToken) -> Result<(), LlamaError> {
        self.unload(model, cancel).await?;
        loop {
            let catalog = self.list(false, cancel).await?;
            match catalog.iter().find(|entry| entry.id == model) {
                Some(entry) if !entry.is_status(status::UNLOADED) => sleep(Duration::from_millis(100), cancel).await?,
                _ => return Ok(()),
            }
        }
    }

    /// Load `model`, reporting stage progress, and return its entry once loaded.
    pub async fn load_and_wait(
        &self,
        model: &str,
        on_progress: &(dyn Fn(LlamaProgress) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<LlamaModelInfo, LlamaError> {
        let state: Mutex<(bool, Option<String>)> = Mutex::new((false, None));
        let on_event = |event: LlamaModelEvent| {
            if event.model != model || (event.event != "model_status" && event.event != "status_change") {
                return;
            }
            let reported = event.data.as_ref().and_then(|data| data.get("status")).and_then(Value::as_str);
            {
                let mut state = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                match reported {
                    Some(status::LOADED) => state.0 = true,
                    Some(status::UNLOADED) => state.1 = Some("Model failed to load".into()),
                    _ => {}
                }
            }
            if let Some(progress) = parse_load_progress(event.data.as_ref()) {
                on_progress(progress);
            }
        };
        let body = async {
            self.load(model, cancel).await?;
            on_progress(LlamaProgress::message("Loading model"));
            loop {
                if cancel.is_cancelled() {
                    return Err(LlamaError::Cancelled);
                }
                let catalog = self.list(false, cancel).await?;
                let entry = catalog.into_iter().find(|entry| entry.id == model);
                let (event_loaded, event_error) = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
                if let Some(entry) = entry.as_ref().filter(|entry| entry.is_status(status::LOADED)) {
                    return Ok(entry.clone());
                }
                if event_loaded && entry.is_none() {
                    return Ok(LlamaModelInfo::new(model, status::LOADED));
                }
                let failed = entry.as_ref().is_some_and(|entry| entry.status.failed == Some(true));
                if failed || event_error.is_some() {
                    let message = match entry.as_ref().and_then(|entry| entry.status.exit_code) {
                        Some(code) => format!("Model exited with code {code}"),
                        None => event_error.unwrap_or_else(|| "Model failed to load".into()),
                    };
                    return Err(LlamaError::Server(message));
                }
                sleep(Duration::from_millis(250), cancel).await?;
            }
        };
        with_watch(self, cancel, on_event, body).await
    }

    /// Ask the router to download `model`, reporting byte progress, and return the reloaded
    /// catalog once the download finished.
    pub async fn download_and_wait(
        &self,
        model: &str,
        on_progress: &(dyn Fn(LlamaProgress) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<Vec<LlamaModelInfo>, LlamaError> {
        // (finished, failure, saw downloading)
        let state: Mutex<(bool, Option<String>, bool)> = Mutex::new((false, None, false));
        let on_event = |event: LlamaModelEvent| {
            if event.model != model {
                return;
            }
            let mut progress = None;
            {
                let mut state = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                match event.event.as_str() {
                    "download_finished" => state.0 = true,
                    "download_failed" => state.1 = Some(error_message(event.data.as_ref(), "Download failed")),
                    "download_progress" => {
                        state.2 = true;
                        progress = parse_download_progress(event.data.as_ref());
                    }
                    _ => {}
                }
            }
            if let Some(progress) = progress {
                on_progress(progress);
            }
        };
        let body = async {
            self.download(model, cancel).await?;
            on_progress(LlamaProgress::message("Downloading model"));
            let mut polls = 0;
            loop {
                if cancel.is_cancelled() {
                    return Err(LlamaError::Cancelled);
                }
                if let Some(failure) = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).1.clone() {
                    return Err(LlamaError::Server(failure));
                }
                let catalog = self.list(false, cancel).await?;
                polls += 1;
                let entry = catalog.iter().find(|entry| entry.id == model);
                let finished = {
                    let mut state = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    if entry.is_some_and(|entry| entry.is_status(status::DOWNLOADING)) {
                        state.2 = true;
                    }
                    state.0
                };
                let saw_downloading = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).2;
                if let Some(entry) = entry.filter(|entry| entry.is_status(status::DOWNLOADING)) {
                    if let Some(progress) = parse_download_progress(entry.status.progress.as_ref()) {
                        on_progress(progress);
                    }
                } else if finished || (entry.is_some() && (saw_downloading || polls >= 2)) {
                    return self.list(true, cancel).await;
                }
                sleep(Duration::from_millis(500), cancel).await?;
            }
        };
        with_watch(self, cancel, on_event, body).await
    }
}
