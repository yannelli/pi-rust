//! Project trust: which folders may load protected `.pi` resources. Ported from upstream
//! `core/trust-manager.ts` (store, detection, options) and `core/project-trust.ts` (decision).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::brand::{APP_NAME, CONFIG_DIR_NAME};
use super::paths::{absolute, canonicalize};

/// Entries under `<cwd>/.pi` that require a trust decision. A bare `.pi` does not.
pub const TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES: [&str; 8] =
    ["settings.json", "mcp.json", "extensions", "skills", "prompts", "themes", "SYSTEM.md", "APPEND_SYSTEM.md"];

/// The `defaultProjectTrust` setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultProjectTrust {
    #[default]
    Ask,
    Always,
    Never,
}

impl DefaultProjectTrust {
    /// `always` and `never` as named; anything else is `ask`, as upstream reads the setting.
    pub fn parse(value: &str) -> Self {
        match value {
            "always" => Self::Always,
            "never" => Self::Never,
            _ => Self::Ask,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectTrustStoreEntry {
    pub path: PathBuf,
    pub decision: bool,
}

/// A change to `trust.json`; `None` removes the saved decision for `path`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectTrustUpdate {
    pub path: PathBuf,
    pub decision: Option<bool>,
}

/// One choice of the trust prompt or `/trust`. Session-only choices have no updates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectTrustOption {
    pub label: String,
    pub trusted: bool,
    pub updates: Vec<ProjectTrustUpdate>,
    pub saved_path: Option<PathBuf>,
}

fn normalize_cwd(cwd: &Path) -> PathBuf {
    canonicalize(&absolute(cwd))
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The parent of the canonical `cwd`, or `None` at the filesystem root.
pub fn project_trust_parent_path(cwd: &Path) -> Option<PathBuf> {
    normalize_cwd(cwd).parent().map(Path::to_path_buf)
}

/// Trust, trust the parent folder, optionally trust for this session, and do not trust.
pub fn project_trust_options(cwd: &Path, include_session_only: bool) -> Vec<ProjectTrustOption> {
    let trust_path = normalize_cwd(cwd);
    let update = |path: &Path, decision| ProjectTrustUpdate { path: path.to_path_buf(), decision };
    let mut options = vec![ProjectTrustOption {
        label: "Trust".into(),
        trusted: true,
        updates: vec![update(&trust_path, Some(true))],
        saved_path: Some(trust_path.clone()),
    }];
    if let Some(parent) = project_trust_parent_path(cwd) {
        options.push(ProjectTrustOption {
            label: format!("Trust parent folder ({})", parent.display()),
            trusted: true,
            updates: vec![update(&parent, Some(true)), update(&trust_path, None)],
            saved_path: Some(parent),
        });
    }
    let session_only = |label: &str, trusted| ProjectTrustOption {
        label: label.into(),
        trusted,
        updates: Vec::new(),
        saved_path: None,
    };
    if include_session_only {
        options.push(session_only("Trust (this session only)", true));
    }
    options.push(ProjectTrustOption {
        label: "Do not trust".into(),
        trusted: false,
        updates: vec![update(&trust_path, Some(false))],
        saved_path: Some(trust_path),
    });
    if include_session_only {
        options.push(session_only("Do not trust (this session only)", false));
    }
    options
}

/// `.pi` entries that need trust in `cwd`, or a project `.agents/skills` in `cwd` or an
/// ancestor. `<home>/.agents/skills` is a user resource and never counts.
pub fn has_trust_requiring_project_resources(cwd: &Path, home: &Path) -> bool {
    let user_agents_skills = canonicalize(&absolute(home)).join(".agents").join("skills");
    let cwd = normalize_cwd(cwd);
    let config_dir = cwd.join(CONFIG_DIR_NAME);
    if TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES.iter().any(|entry| config_dir.join(entry).exists()) {
        return true;
    }
    cwd.ancestors().any(|dir| {
        let agents_skills = dir.join(".agents").join("skills");
        agents_skills != user_agents_skills && agents_skills.exists()
    })
}

type TrustFile = BTreeMap<String, Option<bool>>;

fn read_trust_file(path: &Path) -> anyhow::Result<TrustFile> {
    if !path.exists() {
        return Ok(TrustFile::new());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("Failed to read trust store {}", path.display()))?;
    let parsed: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .with_context(|| format!("Failed to read trust store {}", path.display()))?;
    let Value::Object(entries) = parsed else {
        bail!("Invalid trust store {}: expected an object", path.display());
    };
    let mut data = TrustFile::new();
    for (name, value) in entries {
        let decision = match value {
            Value::Bool(decision) => Some(decision),
            Value::Null => None,
            _ => bail!("Invalid trust store {}: value for {name:?} must be true, false, or null", path.display()),
        };
        data.insert(name, decision);
    }
    Ok(data)
}

fn write_trust_file(path: &Path, data: &TrustFile) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(data)? + "\n")?;
    Ok(())
}

/// A `<trust.json>.lock` directory, compatible with the `proper-lockfile` lock upstream takes.
struct TrustLock(PathBuf);

impl Drop for TrustLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

const LOCK_STALE: Duration = Duration::from_secs(10);

fn acquire_lock(trust_path: &Path) -> anyhow::Result<TrustLock> {
    let lock_path = PathBuf::from(format!("{}.lock", trust_path.display()));
    if let Some(dir) = trust_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    for attempt in 1..=10 {
        match std::fs::create_dir(&lock_path) {
            Ok(()) => return Ok(TrustLock(lock_path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let modified = std::fs::metadata(&lock_path).and_then(|meta| meta.modified());
                let age = modified.ok().and_then(|time| SystemTime::now().duration_since(time).ok());
                if age.is_some_and(|age| age > LOCK_STALE) {
                    let _ = std::fs::remove_dir(&lock_path);
                } else if attempt < 10 {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            Err(error) => return Err(error).context("Failed to acquire trust store lock"),
        }
    }
    bail!("Failed to acquire trust store lock: {} is held by another process", lock_path.display())
}

/// Saved decisions in `<agent-dir>/trust.json`, keyed by canonical directory path. The
/// closest saved ancestor decides.
#[derive(Clone, Debug)]
pub struct ProjectTrustStore {
    trust_path: PathBuf,
}

impl ProjectTrustStore {
    pub fn new(agent_dir: &Path) -> Self {
        Self { trust_path: absolute(agent_dir).join("trust.json") }
    }

    pub fn path(&self) -> &Path {
        &self.trust_path
    }

    pub fn get(&self, cwd: &Path) -> anyhow::Result<Option<bool>> {
        Ok(self.get_entry(cwd)?.map(|entry| entry.decision))
    }

    pub fn get_entry(&self, cwd: &Path) -> anyhow::Result<Option<ProjectTrustStoreEntry>> {
        let _lock = acquire_lock(&self.trust_path)?;
        let data = read_trust_file(&self.trust_path)?;
        Ok(normalize_cwd(cwd).ancestors().find_map(|dir| {
            let decision = (*data.get(&key(dir))?)?;
            Some(ProjectTrustStoreEntry { path: dir.to_path_buf(), decision })
        }))
    }

    pub fn set(&self, cwd: &Path, decision: Option<bool>) -> anyhow::Result<()> {
        self.set_many(&[ProjectTrustUpdate { path: cwd.to_path_buf(), decision }])
    }

    pub fn set_many(&self, updates: &[ProjectTrustUpdate]) -> anyhow::Result<()> {
        let _lock = acquire_lock(&self.trust_path)?;
        let mut data = read_trust_file(&self.trust_path)?;
        for update in updates {
            let name = key(&normalize_cwd(&update.path));
            match update.decision {
                Some(decision) => data.insert(name, Some(decision)),
                None => data.remove(&name),
            };
        }
        write_trust_file(&self.trust_path, &data)
    }
}

/// The answer of a `project_trust` extension handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtensionTrustDecision {
    pub trusted: bool,
    pub remember: bool,
}

/// Inputs of the trust decision for one working directory.
#[derive(Clone, Debug)]
pub struct ProjectTrustRequest<'a> {
    pub cwd: &'a Path,
    pub home: &'a Path,
    /// `--approve` (`-a`) is `Some(true)`, `--no-approve` (`-na`) is `Some(false)`.
    pub trust_override: Option<bool>,
    pub default_project_trust: DefaultProjectTrust,
    pub extension_decision: Option<ExtensionTrustDecision>,
    /// Interactive mode can show the prompt; print, JSON, and RPC modes cannot.
    pub has_ui: bool,
}

/// The trust prompt for interactive mode; pass the choice to [`apply_project_trust_choice`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectTrustPrompt {
    pub message: String,
    pub options: Vec<ProjectTrustOption>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectTrustResolution {
    Decided(bool),
    Prompt(ProjectTrustPrompt),
}

pub fn format_project_trust_prompt(cwd: &Path) -> String {
    format!(
        "Trust project folder?\n{}\n\nThis allows {APP_NAME} to load {CONFIG_DIR_NAME} settings and resources, install missing project packages, and execute project extensions.",
        cwd.display()
    )
}

/// The command-line override, then trust is not needed, then an extension decision, then
/// the closest saved decision, then `defaultProjectTrust`; `ask` prompts only with a UI.
pub fn resolve_project_trust(
    store: &ProjectTrustStore,
    request: &ProjectTrustRequest<'_>,
) -> anyhow::Result<ProjectTrustResolution> {
    if let Some(trusted) = request.trust_override {
        return Ok(ProjectTrustResolution::Decided(trusted));
    }
    if !has_trust_requiring_project_resources(request.cwd, request.home) {
        return Ok(ProjectTrustResolution::Decided(true));
    }
    if let Some(decision) = request.extension_decision {
        if decision.remember {
            store.set(request.cwd, Some(decision.trusted))?;
        }
        return Ok(ProjectTrustResolution::Decided(decision.trusted));
    }
    if let Some(saved) = store.get(request.cwd)? {
        return Ok(ProjectTrustResolution::Decided(saved));
    }
    Ok(match request.default_project_trust {
        DefaultProjectTrust::Always => ProjectTrustResolution::Decided(true),
        DefaultProjectTrust::Never => ProjectTrustResolution::Decided(false),
        DefaultProjectTrust::Ask if !request.has_ui => ProjectTrustResolution::Decided(false),
        DefaultProjectTrust::Ask => ProjectTrustResolution::Prompt(ProjectTrustPrompt {
            message: format_project_trust_prompt(request.cwd),
            options: project_trust_options(request.cwd, true),
        }),
    })
}

/// Save a prompt choice and return whether the project is trusted. Cancelling is untrusted.
pub fn apply_project_trust_choice(
    store: &ProjectTrustStore,
    choice: Option<&ProjectTrustOption>,
) -> anyhow::Result<bool> {
    let Some(choice) = choice else { return Ok(false) };
    if !choice.updates.is_empty() {
        store.set_many(&choice.updates)?;
    }
    Ok(choice.trusted)
}

/// `/trust` text for the saved decision: `none`, `trusted (<path>)`, or inherited.
pub fn format_saved_decision(trust_path: Option<&Path>, entry: Option<&ProjectTrustStoreEntry>) -> String {
    let Some(entry) = entry else { return "none".into() };
    let label = if entry.decision { "trusted" } else { "untrusted" };
    match trust_path {
        Some(path) if path != entry.path => format!("{label} (inherited from {})", entry.path.display()),
        _ => format!("{label} ({})", entry.path.display()),
    }
}

/// The option matching the saved decision, which `/trust` marks and selects first.
pub fn saved_option_index(options: &[ProjectTrustOption], saved: Option<&ProjectTrustStoreEntry>) -> Option<usize> {
    let saved = saved?;
    options.iter().position(|option| {
        option.trusted == saved.decision && option.saved_path.as_deref() == Some(saved.path.as_path())
    })
}
