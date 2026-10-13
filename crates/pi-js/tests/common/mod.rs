#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pi_js::{HostValue, JsError, JsRuntime, JsRuntimeOptions, OutputSink, OutputStream, UncaughtKind};

/// Collects everything written to stdout and stderr, plus uncaught errors.
#[derive(Default)]
pub struct Capture {
    pub stdout: Mutex<String>,
    pub stderr: Mutex<String>,
    pub uncaught: Mutex<Vec<(UncaughtKind, JsError)>>,
}

impl Capture {
    pub fn stdout(&self) -> String {
        self.stdout.lock().unwrap().clone()
    }

    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }
}

impl OutputSink for Capture {
    fn write(&self, stream: OutputStream, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        match stream {
            OutputStream::Stdout => self.stdout.lock().unwrap().push_str(&text),
            OutputStream::Stderr => self.stderr.lock().unwrap().push_str(&text),
        }
    }
}

pub fn options(dir: &Path, capture: &Arc<Capture>) -> JsRuntimeOptions {
    let mut options = JsRuntimeOptions::new(dir);
    options.output = capture.clone();
    let sink = capture.clone();
    options.on_uncaught = Some(Arc::new(move |kind, error| sink.uncaught.lock().unwrap().push((kind, error.clone()))));
    options
}

pub fn runtime(dir: &Path) -> (JsRuntime, Arc<Capture>) {
    let capture = Arc::new(Capture::default());
    let runtime = JsRuntime::new(options(dir, &capture)).unwrap();
    (runtime, capture)
}

pub fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

pub fn tempdir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = pi_js::resolve::canonical(dir.path());
    (dir, root)
}

/// Evaluate an async script body and return its JSON result.
pub async fn eval_json(runtime: &JsRuntime, body: &str) -> serde_json::Value {
    let source = format!("(async () => {{ {body} }})()");
    match runtime.eval(source).await {
        Ok(value) => value.into_json(),
        Err(error) => panic!("script failed: {}", error.display_with_stack()),
    }
}

pub fn json(value: HostValue) -> serde_json::Value {
    value.into_json()
}

/// Run an async function body as a CommonJS module in `root` (so `require` works) and return
/// its JSON result.
pub async fn run_cjs(runtime: &JsRuntime, root: &Path, body: &str) -> serde_json::Value {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let name = format!("case-{}.cjs", NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
    write(root, &name, &format!("module.exports = async function () {{\n{body}\n}};\n"));
    let function = match runtime.import_default(root.join(&name).display().to_string()).await {
        Ok(function) => function,
        Err(error) => panic!("module failed: {}", error.display_with_stack()),
    };
    match function.call(vec![]).await {
        Ok(value) => value.into_json(),
        Err(error) => panic!("script failed: {}", error.display_with_stack()),
    }
}
