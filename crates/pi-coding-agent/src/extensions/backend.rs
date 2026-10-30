//! The engine that runs extensions: `--extension-runtime`, then `PI_EXTENSION_RUNTIME`, then the
//! `extensionRuntime` setting. Node falls back to QuickJS with one warning per load.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_js::JsBackend;
use pi_js::node_backend::{MIN_NODE_MAJOR, NODE_PATH_ENV, find_executable, probe};
use serde_json::Value;

use super::loader::LoadEntry;
use super::runner::ExtensionRunner;
use crate::agent_session::{AgentSessionServices, RuntimeDiagnostic};
pub use crate::settings::schema::ExtensionRuntime;

/// Environment variable that selects the extension runtime when no CLI flag does.
pub const RUNTIME_ENV: &str = "PI_EXTENSION_RUNTIME";

/// Directory levels under a package root searched for `.node` addons.
const ADDON_SCAN_DEPTH: usize = 4;
/// Directory levels above an entry file searched for its `package.json`.
const MANIFEST_SEARCH_DEPTH: usize = 3;
/// Directory entries one package scan reads at most.
const ADDON_SCAN_BUDGET: usize = 20_000;

/// The requested runtime: `cli`, then `env`, then `settings`. An invalid `env` value is skipped
/// with a warning.
pub fn resolve_runtime(
    cli: Option<ExtensionRuntime>,
    env: Option<&str>,
    settings: ExtensionRuntime,
) -> (ExtensionRuntime, Option<String>) {
    if let Some(runtime) = cli {
        return (runtime, None);
    }
    let Some(value) = env.map(str::trim).filter(|value| !value.is_empty()) else { return (settings, None) };
    match ExtensionRuntime::parse(value) {
        Some(runtime) => (runtime, None),
        None => {
            let names = ExtensionRuntime::NAMES.join(", ");
            (settings, Some(format!("Ignoring {RUNTIME_ENV}=\"{value}\". Valid values: {names}")))
        }
    }
}

/// Whether a loaded extension package sets `package.json` `pi.runtime: "node"` or ships a
/// `.node` addon. Entries outside a package do not count.
pub fn wants_node(entries: &[LoadEntry]) -> bool {
    let roots: BTreeSet<PathBuf> = entries.iter().filter_map(package_root).collect();
    roots.iter().any(|root| declares_node(root) || has_addon(root))
}

fn package_root(entry: &LoadEntry) -> Option<PathBuf> {
    let start =
        if entry.resolved_path.is_dir() { entry.resolved_path.as_path() } else { entry.resolved_path.parent()? };
    start
        .ancestors()
        .take(MANIFEST_SEARCH_DEPTH)
        .take_while(|dir| dir.file_name().is_none_or(|name| name != "node_modules"))
        .find(|dir| dir.join("package.json").is_file())
        .map(Path::to_path_buf)
}

fn declares_node(root: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(root.join("package.json")) else { return false };
    let manifest: Value = serde_json::from_str(&text).unwrap_or_default();
    manifest["pi"]["runtime"].as_str().is_some_and(|runtime| runtime.eq_ignore_ascii_case("node"))
}

fn has_addon(root: &Path) -> bool {
    let mut budget = ADDON_SCAN_BUDGET;
    scan_for_addon(root, ADDON_SCAN_DEPTH, &mut budget)
}

fn scan_for_addon(dir: &Path, depth_left: usize, budget: &mut usize) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        if kind.is_file() && path.extension().is_some_and(|extension| extension == "node") {
            return true;
        }
        if kind.is_dir() && depth_left > 0 && !skip_dir(dir, &entry.file_name()) {
            subdirs.push(path);
        }
    }
    subdirs.iter().any(|subdir| scan_for_addon(subdir, depth_left - 1, budget))
}

fn skip_dir(parent: &Path, name: &std::ffi::OsStr) -> bool {
    name == ".git" || (name == ".bin" && parent.file_name().is_some_and(|parent| parent == "node_modules"))
}

/// The backend for `runtime`, or why Node cannot run when it was asked for.
pub fn select_backend(runtime: ExtensionRuntime, entries: &[LoadEntry]) -> Result<JsBackend, String> {
    match runtime {
        ExtensionRuntime::QuickJs => Ok(JsBackend::QuickJs),
        ExtensionRuntime::Auto if !wants_node(entries) => Ok(JsBackend::QuickJs),
        ExtensionRuntime::Node | ExtensionRuntime::Auto => node_backend(),
    }
}

fn node_backend() -> Result<JsBackend, String> {
    let executable = find_executable()
        .ok_or_else(|| format!("Node {MIN_NODE_MAJOR} or later was not found on PATH or in {NODE_PATH_ENV}"))?;
    probe(&executable).map_err(|error| error.to_string())?;
    Ok(JsBackend::Node { executable })
}

fn fallback_warning(reason: &str) -> RuntimeDiagnostic {
    RuntimeDiagnostic::warning(format!("Cannot run extensions on Node: {reason}. Using QuickJS."))
}

/// Start the runner on the resolved backend. `requested` is the CLI choice, kept for reloads.
pub(crate) async fn start_runner(
    services: &AgentSessionServices,
    requested: Option<ExtensionRuntime>,
    entries: &[LoadEntry],
) -> (anyhow::Result<Arc<ExtensionRunner>>, Vec<RuntimeDiagnostic>) {
    let env = pi_ai::env::process_env_value(RUNTIME_ENV);
    let (runtime, env_warning) = resolve_runtime(requested, env.as_deref(), services.settings.extension_runtime());
    let mut diagnostics: Vec<RuntimeDiagnostic> = env_warning.into_iter().map(RuntimeDiagnostic::warning).collect();
    let backend = select_backend(runtime, entries).unwrap_or_else(|reason| {
        diagnostics.push(fallback_warning(&reason));
        JsBackend::QuickJs
    });
    let start =
        |backend| ExtensionRunner::start(&services.cwd, &services.agent_dir, services.model_runtime.clone(), backend);
    let started = match start(backend.clone()).await {
        Err(error) if backend != JsBackend::QuickJs => {
            diagnostics.push(fallback_warning(&format!("{error:#}")));
            start(JsBackend::QuickJs).await
        }
        started => started,
    };
    if let (Ok(runner), Some(requested)) = (&started, requested) {
        runner.set_requested_runtime(requested);
    }
    (started, diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::source_info::SourceInfo;
    use ExtensionRuntime::{Auto, Node, QuickJs};

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn precedence_is_cli_then_env_then_settings() {
        assert_eq!(resolve_runtime(Some(QuickJs), Some("node"), Auto), (QuickJs, None));
        assert_eq!(resolve_runtime(None, Some(" Node "), Auto), (Node, None));
        assert_eq!(resolve_runtime(None, Some(""), Auto), (Auto, None));
        assert_eq!(resolve_runtime(None, None, QuickJs), (QuickJs, None));
        let (runtime, warning) = resolve_runtime(None, Some("deno"), Node);
        assert_eq!(runtime, Node);
        assert!(warning.unwrap().contains("PI_EXTENSION_RUNTIME=\"deno\""));
    }

    #[test]
    fn auto_detects_runtime_field_and_native_addons() {
        let root = tempfile::tempdir().unwrap();
        let plain = root.path().join("plain");
        write(&plain.join("package.json"), r#"{"name": "plain"}"#);
        write(&plain.join("extensions/index.ts"), "export default () => {}");
        write(&plain.join("node_modules/.bin/tool.node"), "");
        let declared = root.path().join("declared");
        write(&declared.join("package.json"), r#"{"pi": {"runtime": "node"}}"#);
        write(&declared.join("index.ts"), "");
        let addon = root.path().join("addon");
        write(&addon.join("package.json"), "{}");
        write(&addon.join("src/ext/index.ts"), "");
        write(&addon.join("node_modules/sqlite/build/Release/sqlite.node"), "");
        let deep = root.path().join("deep");
        write(&deep.join("package.json"), "{}");
        write(&deep.join("index.ts"), "");
        write(&deep.join("a/b/c/d/e/too-deep.node"), "");
        let loose = root.path().join("loose/one.ts");
        write(&loose, "");
        write(&root.path().join("loose/x.node"), "");

        let installed = root.path().join("npm/node_modules/@scope/native");
        write(&installed.join("package.json"), r#"{"name": "@scope/native"}"#);
        write(&installed.join("extensions/index.ts"), "");
        write(&installed.join("prebuilds/darwin-arm64/native.node"), "");
        write(&root.path().join("npm/package.json"), r#"{"pi": {"runtime": "node"}}"#);
        let other = root.path().join("npm/node_modules/plain-ext/index.ts");
        write(&other, "");

        let entry = |path: PathBuf| vec![LoadEntry::file(path)];
        let mut packaged = LoadEntry::file(installed.join("extensions/index.ts"));
        let base_dir = Some(installed.join("extensions").display().to_string());
        let (path, source) = (packaged.path.clone(), "npm:@scope/native".to_string());
        packaged.source_info =
            Some(SourceInfo { path, source, scope: Default::default(), origin: Default::default(), base_dir });
        assert!(wants_node(&[packaged]));
        assert!(!wants_node(&entry(other)), "the walk stops at node_modules");
        assert!(!wants_node(&entry(plain.join("extensions/index.ts"))));
        assert!(wants_node(&entry(declared.join("index.ts"))));
        assert!(wants_node(&entry(addon.join("src/ext/index.ts"))));
        assert!(!wants_node(&entry(deep.join("index.ts"))));
        assert!(!wants_node(&entry(loose)));
        assert!(wants_node(&[LoadEntry::file(plain.join("extensions/index.ts")), LoadEntry::file(declared)]));
        assert_eq!(select_backend(Auto, &entry(plain.join("extensions/index.ts"))), Ok(JsBackend::QuickJs));
        assert_eq!(select_backend(QuickJs, &entry(addon.join("src/ext/index.ts"))), Ok(JsBackend::QuickJs));
    }
}
