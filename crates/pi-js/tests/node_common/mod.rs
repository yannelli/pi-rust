#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pi_js::{JsBackend, JsRuntime, JsRuntimeOptions};

use super::common::{Capture, options};

/// Node 22 or later from `PI_NODE_PATH` or `PATH`; `None` (and a message) skips the test.
pub fn node() -> Option<PathBuf> {
    let Some(path) = pi_js::node_backend::find_executable() else {
        eprintln!("skipping: no node binary on PATH or in PI_NODE_PATH; the Node backend needs Node 22+");
        return None;
    };
    match pi_js::node_backend::probe(&path) {
        Ok(_) => Some(path),
        Err(error) => {
            eprintln!("skipping: {}", error.message);
            None
        }
    }
}

pub fn node_runtime_with(mut options: JsRuntimeOptions) -> Option<JsRuntime> {
    options.backend = JsBackend::Node { executable: node()? };
    Some(JsRuntime::new(options).expect("the Node backend starts"))
}

pub fn node_runtime(dir: &Path) -> Option<(JsRuntime, Arc<Capture>)> {
    let capture = Arc::new(Capture::default());
    let runtime = node_runtime_with(options(dir, &capture))?;
    Some((runtime, capture))
}

/// Poll `ready` until it holds; panics after 20 seconds.
pub async fn wait_for(ready: impl Fn() -> bool) {
    for _ in 0..2000 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached in 20 seconds");
}
