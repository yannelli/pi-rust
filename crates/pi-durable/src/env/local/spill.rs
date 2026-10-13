//! The spill of a command: its complete raw output in a temporary file, once the output crosses a threshold.

use std::sync::Arc;

use pi_chord::Context;
use tokio::io::{AsyncWriteExt, BufWriter};

use super::LocalExecutionEnv;
use crate::env::{ExecutionError, ExecutionErrorCode, FileSystem, ShellSpillOptions, TempFileOptions};

const SPILL_BUFFER: usize = 1024 * 1024;

/// Output seen before the spill starts is counted against the thresholds and kept for the spill's prefix.
pub(crate) struct Spill {
    options: ShellSpillOptions,
    seen_bytes: u64,
    seen_newlines: u64,
    prefix: Vec<Vec<u8>>,
    started: bool,
    file: Option<BufWriter<tokio::fs::File>>,
    pub path: Option<String>,
    pub error: Option<ExecutionError>,
}

fn spill_failure(message: String, cause: Option<Arc<dyn std::error::Error + Send + Sync>>) -> ExecutionError {
    let error = ExecutionError::new(
        ExecutionErrorCode::Unknown,
        format!("Failed to preserve complete shell output: {message}"),
    );
    match cause {
        Some(cause) => error.with_cause(cause),
        None => error,
    }
}

impl Spill {
    pub fn new(options: ShellSpillOptions) -> Self {
        Self {
            options,
            seen_bytes: 0,
            seen_newlines: 0,
            prefix: Vec::new(),
            started: false,
            file: None,
            path: None,
            error: None,
        }
    }

    /// Count or write one raw chunk. Returns false when the spill failed just now, so the command must be killed.
    pub async fn feed(&mut self, chunk: &[u8], env: &LocalExecutionEnv, context: &Context) -> bool {
        if chunk.is_empty() || self.error.is_some() {
            return true;
        }
        if self.started {
            return self.write(chunk).await;
        }
        self.seen_bytes += chunk.len() as u64;
        self.seen_newlines += chunk.iter().filter(|&&byte| byte == b'\n').count() as u64;
        let lines = self.seen_newlines + u64::from(chunk.last() != Some(&b'\n'));
        if self.seen_bytes <= self.options.after_bytes && lines <= self.options.after_lines {
            self.prefix.push(chunk.to_vec());
            return true;
        }
        self.started = true;
        let options = TempFileOptions { prefix: Some("pi-output-".into()), suffix: Some(".log".into()) };
        let path = match env.create_temp_file(options, context).await {
            Ok(path) => path,
            Err(error) => {
                self.error = Some(spill_failure(error.message.clone(), Some(Arc::new(error))));
                return false;
            }
        };
        let file = tokio::fs::OpenOptions::new().append(true).create(true).open(&path).await;
        self.path = Some(path);
        match file {
            Ok(file) => self.file = Some(BufWriter::with_capacity(SPILL_BUFFER, file)),
            Err(error) => {
                self.error = Some(spill_failure(error.to_string(), Some(Arc::new(error))));
                return false;
            }
        }
        for prefix in std::mem::take(&mut self.prefix) {
            if !self.write(&prefix).await {
                return false;
            }
        }
        self.write(chunk).await
    }

    async fn write(&mut self, chunk: &[u8]) -> bool {
        let Some(file) = self.file.as_mut() else { return true };
        match file.write_all(chunk).await {
            Ok(()) => true,
            Err(error) => {
                self.file = None;
                self.error = Some(spill_failure(error.to_string(), Some(Arc::new(error))));
                false
            }
        }
    }

    /// Flush and close the spill file.
    pub async fn finish(&mut self) {
        if let Some(mut file) = self.file.take() {
            if let Err(error) = file.shutdown().await {
                self.error.get_or_insert_with(|| spill_failure(error.to_string(), Some(Arc::new(error))));
            }
        }
    }
}
