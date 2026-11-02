//! Files load in a child extension host; factories run in the caller host.
//! The child host mirrors factory calls after loading the files (upstream `loadFinalExtensionSet`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use pi_js::{HostValue, JsError, JsRef};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::super::backend::{ExtensionRuntime, start_runner};
use super::super::loader::LoadEntry;
use super::super::runner::{ExtensionLoadError, ExtensionRunner};
use super::super::vendor::{specifier, vendor_root};
use crate::agent_session::{AgentSessionServices, lock};
use crate::resources::ResourceLoader;
use crate::settings::SettingsManager;

/// Host calls that run one at a time, in the order they were sent.
pub(crate) type Queue = mpsc::UnboundedSender<Vec<Value>>;

fn queue(host: JsRef) -> Queue {
    let (sender, mut receiver) = mpsc::unbounded_channel::<Vec<Value>>();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            while let Some(args) = receiver.recv().await {
                let _ = host.call_method("sdk", args.into_iter().map(HostValue::Json).collect()).await;
            }
        });
    }
    sender
}

/// How a child host reaches the host that owns its loader.
pub(crate) struct ParentLink {
    pub(crate) parent: Weak<ExtensionRunner>,
    pub(crate) loader: u64,
    /// The factory group of the parent host (`sdk_extensions.js`).
    pub(crate) group: u64,
    /// The parent's target of the bound child session, or 0.
    pub(crate) target: AtomicU64,
    pub(crate) to_parent: Queue,
    pub(crate) to_child: Queue,
}

/// A child extension host. Dropping it ends the host and removes what it registered.
pub(crate) struct Hosted(pub(crate) Arc<ExtensionRunner>);

impl Hosted {
    pub(crate) fn group(&self) -> u64 {
        self.0.parent.get().map_or(0, |link| link.group)
    }
}

impl Drop for Hosted {
    fn drop(&mut self) {
        let child = &self.0;
        if let Some(link) = child.parent.get()
            && let Some(parent) = link.parent.upgrade()
        {
            lock(&parent.targets).remove(&link.target.load(Ordering::SeqCst));
        }
        child.remove_tools();
        child.unregister_models();
        child.shutdown();
    }
}

/// The extensions of the last `reload()`, until a session takes them.
#[derive(Default)]
pub(super) struct Loaded {
    hosted: Option<Hosted>,
    pub(super) group: u64,
    /// Mirror descriptors of the loaded factories, by index.
    pub(super) factories: Vec<Value>,
    pub(super) mirrored: usize,
    /// Resolved paths that loaded and paths that failed in the pre-trust pass.
    preloaded: Option<(Vec<PathBuf>, Vec<String>)>,
    taken: bool,
}

impl Loaded {
    pub(super) fn child(&self, group: u64) -> Option<Arc<ExtensionRunner>> {
        self.hosted.as_ref().filter(|hosted| hosted.group() == group).map(|hosted| hosted.0.clone())
    }
}

pub(super) struct Reload {
    pub(super) id: u64,
    pub(super) loader: Arc<Mutex<ResourceLoader>>,
    pub(super) settings: Arc<SettingsManager>,
    pub(super) cwd: PathBuf,
    pub(super) agent_dir: PathBuf,
    pub(super) entries: Vec<LoadEntry>,
    pub(super) warnings: Vec<Value>,
}

fn errors_json(errors: &[ExtensionLoadError]) -> Vec<Value> {
    errors.iter().map(|failure| json!({"path": failure.path, "error": failure.error})).collect()
}

async fn load_new(child: &ExtensionRunner, entries: &[LoadEntry]) -> Vec<ExtensionLoadError> {
    let before = child.load_errors().len();
    child.load_entries(entries).await;
    child.load_errors().split_off(before)
}

/// Start a child host on QuickJS, which runs the mirrors and the bus bridge; runtime warnings go to
/// `warnings`.
async fn start(
    parent: &Arc<ExtensionRunner>,
    reload: &Reload,
    group: u64,
    warnings: &mut Vec<Value>,
) -> anyhow::Result<Arc<ExtensionRunner>> {
    let services = AgentSessionServices {
        cwd: reload.cwd.clone(),
        agent_dir: reload.agent_dir.clone(),
        model_runtime: parent.model_runtime().clone(),
        settings: reload.settings.clone(),
        resource_loader: reload.loader.clone(),
        diagnostics: Vec::new(),
    };
    let (started, diagnostics) = start_runner(&services, Some(ExtensionRuntime::QuickJs), &reload.entries).await;
    warnings.extend(diagnostics.into_iter().map(|warning| json!({"path": "<runtime>", "warning": warning.message})));
    let child = started?;
    let link = ParentLink {
        parent: Arc::downgrade(parent),
        loader: reload.id,
        group,
        target: AtomicU64::new(0),
        to_parent: queue(parent.host().clone()),
        to_child: queue(child.host().clone()),
    };
    let _ = child.parent.set(link);
    child
        .call_json("sdk", vec![json!("bridge")])
        .await
        .map_err(|failure| anyhow::anyhow!(failure.display_with_stack()))?;
    Ok(child)
}

/// Load the files of one reload phase: `pretrust`, `final` (reusing what `pretrust` loaded), or
/// `full`. A child host starts when there are files or `factories`.
pub(super) async fn load(
    parent: &Arc<ExtensionRunner>,
    mut reload: Reload,
    phase: &str,
    factories: bool,
) -> Result<Value, JsError> {
    let (old, reused) = {
        let mut state = lock(&parent.resources);
        let loaded = &mut state.entry(reload.id)?.loaded;
        let preloaded = loaded.preloaded.take();
        match preloaded {
            Some(preloaded) if phase == "final" && !loaded.taken => {
                (None, Some((preloaded, loaded.hosted.as_ref().map(|hosted| hosted.0.clone()), loaded.group)))
            }
            _ => (Some(std::mem::take(loaded)), None),
        }
    };
    drop(old);
    let paths: Vec<String> = reload.entries.iter().map(|entry| entry.path.clone()).collect();
    let mut warnings = std::mem::take(&mut reload.warnings);
    let reusing = reused.is_some();
    let (to_load, mut child, mut group) = match reused {
        Some(((loaded, failed), child, group)) => {
            let remaining = reload
                .entries
                .iter()
                .filter(|entry| !loaded.contains(&entry.resolved_path) && !failed.contains(&entry.path));
            (remaining.cloned().collect::<Vec<_>>(), child, group)
        }
        None => (reload.entries.clone(), None, 0),
    };
    let (mut errors, mut started) = (Vec::new(), None);
    if child.is_none() && (!to_load.is_empty() || factories) {
        group = parent.next_id();
        match start(parent, &reload, group, &mut warnings).await {
            Ok(runner) => {
                child = Some(runner.clone());
                started = Some(Hosted(runner));
            }
            Err(failure) => errors.push(json!({
                "path": "<runtime>",
                "error": format!("Failed to start the extension host: {failure:#}"),
            })),
        }
    }
    let mut preloaded = None;
    if let Some(child) = &child {
        let failed = load_new(child, &to_load).await;
        errors.extend(errors_json(&failed));
        if phase == "pretrust" {
            let failed: Vec<String> = failed.into_iter().map(|failure| failure.path).collect();
            let loaded = to_load.iter().filter(|entry| !failed.contains(&entry.path));
            preloaded = Some((loaded.map(|entry| entry.resolved_path.clone()).collect(), failed));
        }
    }
    let mut state = lock(&parent.resources);
    state.hosts.retain(|_, host| host.strong_count() > 0);
    if let Some(started) = &started {
        state.hosts.insert(group, Arc::downgrade(&started.0));
    }
    let loaded = &mut state.entry(reload.id)?.loaded;
    if started.is_some() {
        loaded.hosted = started;
        loaded.group = group;
    }
    loaded.preloaded = preloaded;
    Ok(json!({"group": group, "reused": reusing, "paths": paths, "errors": errors, "warnings": warnings}))
}

/// Load the mirrors of factories not mirrored yet, and describe the child host's file extensions.
pub(super) async fn finish(parent: &Arc<ExtensionRunner>, id: u64) -> Result<Value, JsError> {
    let (child, group, mirrors) = {
        let mut state = lock(&parent.resources);
        let loaded = &mut state.entry(id)?.loaded;
        let Some(child) = loaded.hosted.as_ref().map(|hosted| hosted.0.clone()) else {
            return Ok(json!({"extensions": [], "errors": []}));
        };
        let mirrors = loaded.factories.iter().enumerate().skip(loaded.mirrored);
        let mirrors: Vec<(usize, String)> =
            mirrors.map(|(index, mirror)| (index, mirror["path"].as_str().unwrap_or_default().to_string())).collect();
        loaded.mirrored = loaded.factories.len();
        (child, loaded.group, mirrors)
    };
    let mut errors = Vec::new();
    if !mirrors.is_empty() {
        let module = json!(specifier(&vendor_root(), "host/sdk_mirror.js"));
        let (mut files, mut entries) = (Vec::new(), Vec::new());
        for (index, path) in mirrors {
            let file = vendor_root().join("sdk-mirror").join(format!("{group}-{index}.js"));
            let source = format!("import {{ mirror }} from {module};\nexport default (pi) => mirror(pi, {index});\n");
            files.push((file.clone(), source));
            entries.push(LoadEntry { path, resolved_path: file, source_info: None, native: None });
        }
        child.js().register_source_files(files);
        errors = errors_json(&load_new(&child, &entries).await);
    }
    let extensions = child.call_json("sdk", vec![json!("snapshot")]).await?;
    Ok(json!({"extensions": extensions, "errors": errors}))
}

/// Keep and order the child host's extensions as the final result lists them. Their models bind
/// when a session takes them, to that session's runtime (`ops_sdk_create`).
pub(super) async fn order(parent: &Arc<ExtensionRunner>, id: u64, paths: Value) -> Result<Value, JsError> {
    let child = {
        let mut state = lock(&parent.resources);
        state.entry(id)?.loaded.hosted.as_ref().map(|hosted| hosted.0.clone())
    };
    if let Some(child) = child {
        child.call_json("sdk", vec![json!("order"), paths]).await?;
    }
    Ok(Value::Null)
}

/// Hand the child host of the last reload to a new session. One reload serves one session, since a
/// runner binds a single session and its handlers and tools run against that session.
pub(super) fn take(parent: &ExtensionRunner, loaded: &mut Loaded) -> Result<Option<(Hosted, u64)>, JsError> {
    if loaded.taken {
        return Err(JsError::unsupported(
            "A second AgentSession on the extensions of one DefaultResourceLoader reload (call reload() first)",
        ));
    }
    let Some(hosted) = loaded.hosted.take() else { return Ok(None) };
    loaded.taken = true;
    let target = parent.next_id();
    if let Some(link) = hosted.0.parent.get() {
        link.target.store(target, Ordering::SeqCst);
    }
    Ok(Some((hosted, target)))
}
