//! The interactive mode on a virtual terminal over the faux provider, and a signed local
//! marketplace, for the `/extensions` browser tests that compile inside the crate.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use pi_ai::auth::InMemoryCredentialStore;
use pi_ai::env::{EnvLookup, credential_env_vars};
use pi_ai::providers::faux::FauxProvider;
use pi_tui::testing::VirtualTerminal;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::agent_session::AgentSession;
use crate::app::{AppOptions, create_app_runtime};
use crate::interactive::{InteractiveMode, InteractiveOptions};
use crate::keybindings::KeybindingsManager;
use crate::packages::marketplace::host_target;
use crate::packages::{Marketplace, PackageManager, PackageManagerOptions};
use crate::platform::test_http::{TestReply, TestServer, client};
use crate::session::SessionManager;
use crate::settings::SettingsManager;

static NEXT_API: AtomicU64 = AtomicU64::new(1);
const TIMEOUT: Duration = Duration::from_secs(15);
pub const BINARY: &str = "pi-rust-ext";

pub struct Ui {
    pub root: TempDir,
    pub agent_dir: PathBuf,
    pub terminal: Rc<RefCell<VirtualTerminal>>,
    pub mode: Rc<InteractiveMode>,
}

/// Starts the mode on a 160x40 terminal with `agent_files` under the agent directory, then
/// waits for the footer. Call inside a `LocalSet`.
pub async fn start(agent_files: &[(&str, &str)]) -> Ui {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    for (path, content) in agent_files {
        let path = agent_dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    std::fs::create_dir_all(&agent_dir).unwrap();
    let api = format!("faux-extensions-browser-{}", NEXT_API.fetch_add(1, Ordering::Relaxed));
    let models = json!({"providers": {"local": {"baseUrl": "http://unused", "api": api, "apiKey": "test-key",
        "models": [{"id": "tiny", "contextWindow": 128000, "maxTokens": 4000}]}}});
    std::fs::write(agent_dir.join("models.json"), models.to_string()).unwrap();
    pi_ai::register_api_provider(api.clone(), FauxProvider::with_delay(Vec::new(), Duration::ZERO));
    let settings = json!({"defaultProvider": "local", "defaultModel": "tiny", "theme": "dark",
        "enableInstallTelemetry": false, "lastChangelogVersion": "999.0.0"});
    let hidden = credential_env_vars();
    let env: Arc<dyn EnvLookup> =
        Arc::new(move |name: &str| if hidden.contains(name) { None } else { std::env::var(name).ok() });
    let mut app = AppOptions {
        agent_dir: agent_dir.clone(),
        settings: Some(Arc::new(SettingsManager::in_memory(settings))),
        credentials: Some(Arc::new(InMemoryCredentialStore::new(BTreeMap::new()))),
        credential_env: Some(env),
        ..AppOptions::default()
    };
    app.resources.home = Some(agent_dir.join("home"));
    let manager = SessionManager::create(&cwd, Some(&agent_dir.join("sessions")));
    let runtime = create_app_runtime(app, manager).await.unwrap();
    let model = runtime.session().model().expect("session model");
    assert_eq!(model.api, api, "the session must use the faux test model");
    KeybindingsManager::create(&agent_dir).install();
    let terminal = VirtualTerminal::shared(160, 40);
    let options = InteractiveOptions {
        terminal: Some(terminal.clone()),
        agent_dir: Some(agent_dir.clone()),
        skip_update_checks: true,
        ..InteractiveOptions::default()
    };
    let mode = InteractiveMode::new(Arc::new(runtime), options);
    let running = mode.clone();
    tokio::task::spawn_local(async move {
        let _ = running.run().await;
    });
    let ui = Ui { root, agent_dir, terminal, mode };
    ui.wait_until("startup", |ui| ui.mode.is_ready()).await;
    ui.wait_for("tiny").await;
    ui
}

impl Ui {
    pub fn cwd(&self) -> PathBuf {
        self.root.path().join("work")
    }

    pub fn session(&self) -> Arc<AgentSession> {
        self.mode.current_session()
    }

    /// A manager over the session's settings, so browser writes show in `session().settings()`.
    pub fn package_manager(&self) -> PackageManager {
        let settings = self.session().settings().clone();
        let mut options = PackageManagerOptions::new(&self.cwd(), &self.agent_dir, settings);
        options.home = self.agent_dir.join("home");
        options.http_client = Some(client());
        options.offline = Some(false);
        PackageManager::new(options)
    }

    pub fn screen(&self) -> String {
        self.terminal.borrow().viewport().join("\n")
    }

    pub async fn wait_until(&self, what: &str, mut done: impl FnMut(&Ui) -> bool) {
        let started = Instant::now();
        while !done(self) {
            assert!(started.elapsed() < TIMEOUT, "timed out waiting for {what}; screen:\n{}", self.screen());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn wait_for(&self, text: &str) {
        self.wait_until(text, |ui| ui.screen().contains(text)).await;
    }

    /// Sends keys, then lets the UI handle them.
    pub async fn send(&self, keys: &str) {
        self.terminal.borrow().send_input(keys);
        tokio::time::sleep(Duration::from_millis(60)).await;
    }

    /// Paths of the extensions the current session loaded.
    pub fn loaded_extensions(&self) -> Vec<String> {
        self.session().extension_runner().map(|runner| runner.extension_paths()).unwrap_or_default()
    }
}

/// Runs `test` on a `LocalSet`, as the interactive mode needs.
pub async fn local<F: Future<Output = ()>>(test: F) {
    tokio::task::LocalSet::new().run_until(test).await;
}

/// Signed indexes and binary archives on a local HTTP server.
pub struct MockMarketplace {
    pub server: TestServer,
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    key: Ed25519KeyPair,
}

impl MockMarketplace {
    pub async fn start() -> Self {
        let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let served = files.clone();
        let server = TestServer::start(move |request| match served.lock().unwrap().get(request.path()) {
            Some(body) => TestReply::bytes(200, body.clone()),
            None => TestReply::status(404, "missing"),
        })
        .await;
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        Self { server, files, key }
    }

    pub fn put(&self, path: &str, body: Vec<u8>) {
        self.files.lock().unwrap().insert(path.to_string(), body);
    }

    pub fn signature(&self, body: &[u8]) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.encode(self.key.sign(body).as_ref()).into_bytes()
    }

    /// Serves the signed `index` as the latest release and at `/<tag>/index.json`.
    pub fn publish(&self, index: &Value) {
        let body = serde_json::to_vec_pretty(index).unwrap();
        let tag = index["tag"].as_str().unwrap();
        for path in [format!("/{tag}/index.json"), "/index.json".to_string()] {
            self.put(&format!("{path}.sig"), self.signature(&body));
            self.put(&path, body.clone());
        }
    }

    /// An index for the host target whose binary archive holds `content`.
    pub fn release(&self, tag: &str, names: &[&str], content: &str) -> Value {
        let target = host_target().unwrap();
        let windows = target.contains("-windows-");
        let archive = binary_archive(target, windows, content);
        let path = format!("/{tag}/{BINARY}-{target}.{}", if windows { "zip" } else { "tar.gz" });
        let sha256: String = ring::digest::digest(&ring::digest::SHA256, &archive)
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let asset = json!({ "url": format!("{}{path}", self.server.url), "sha256": sha256, "size": archive.len() });
        self.put(&path, archive);
        let entries: Vec<Value> = names
            .iter()
            .map(|name| {
                json!({
                    "name": name, "displayName": name, "description": format!("The {name} extension"),
                    "version": tag.trim_start_matches('v'), "binary": BINARY, "args": [name], "tags": ["test"],
                    "platforms": [target],
                })
            })
            .collect();
        json!({
            "schemaVersion": 1, "repository": "yannelli/pi-rust-extensions", "tag": tag, "protocolVersion": 1,
            "minPiVersion": "0.1.0", "binaries": { BINARY: { target: asset } }, "extensions": entries,
        })
    }

    pub fn marketplace(&self, agent_dir: &Path, offline: bool) -> Marketplace {
        let key: [u8; 32] = self.key.public_key().as_ref().try_into().unwrap();
        Marketplace::new(agent_dir.to_path_buf(), client(), offline)
            .with_public_key(key)
            .with_base_url(&self.server.url)
    }
}

/// The release archive layout: `<binary>-<target>/<binary>[.exe]`.
fn binary_archive(target: &str, windows: bool, content: &str) -> Vec<u8> {
    let folder = format!("{BINARY}-{target}");
    if windows {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer.start_file(format!("{folder}/{BINARY}.exe"), zip::write::SimpleFileOptions::default()).unwrap();
        writer.write_all(content.as_bytes()).unwrap();
        return writer.finish().unwrap().into_inner();
    }
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    builder.append_data(&mut header, format!("{folder}/{BINARY}"), content.as_bytes()).unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}
