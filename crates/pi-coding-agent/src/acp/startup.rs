//! The startup info block of a new session (upstream `buildStartupInfo` and
//! `buildUpdateNotice`): pi's version, context file, skills, prompts, and extensions.

use std::path::Path;

use serde_json::Value;

use crate::platform::version_check::LatestPiRelease;

/// What the startup info lists.
pub struct StartupInfoInput<'a> {
    pub cwd: &'a Path,
    pub agent_dir: &'a Path,
    pub home: &'a Path,
    pub version: &'a str,
    pub update_notice: Option<&'a str>,
}

/// "New version available" for a newer release than `installed`.
pub fn update_notice(release: Option<&LatestPiRelease>, installed: &str) -> Option<String> {
    let release = release?;
    let latest = release.version.trim_start_matches(['v', 'V']);
    let installed = installed.trim_start_matches(['v', 'V']);
    let newer = match (semver::Version::parse(latest), semver::Version::parse(installed)) {
        (Ok(latest), Ok(installed)) => latest > installed,
        _ => false,
    };
    newer.then(|| format!("New version available: v{latest} (installed v{installed}). Run: `pi update`"))
}

fn sorted_entries(dir: &Path) -> Vec<std::fs::DirEntry> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).map(|entries| entries.flatten().collect()).unwrap_or_default();
    entries.sort_by_key(|entry| entry.file_name());
    entries
}

/// `.md` files directly under `root`, then every `SKILL.md` below it.
fn skills_from_root(root: &Path, out: &mut Vec<String>) {
    for entry in sorted_entries(root) {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if entry.file_type().is_ok_and(|kind| kind.is_file()) && name.ends_with(".md") {
            out.push(entry.path().display().to_string());
        }
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in sorted_entries(&dir) {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "node_modules" || name == ".git" {
                continue;
            }
            let Ok(meta) = std::fs::metadata(entry.path()) else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() && name == "SKILL.md" {
                out.push(entry.path().display().to_string());
            }
        }
    }
}

fn settings_packages(path: &Path) -> Vec<String> {
    let parsed = std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let packages = parsed.as_ref().and_then(|settings| settings["packages"].as_array()).cloned().unwrap_or_default();
    packages
        .iter()
        .map(|package| match package {
            Value::String(text) => text.clone(),
            Value::Object(object) => object.get("source").and_then(Value::as_str).unwrap_or_default().to_string(),
            other => other.to_string(),
        })
        .filter(|package| !package.is_empty())
        .map(|package| if package.starts_with("npm:") { format!("{package}\n  - index.ts") } else { package })
        .collect()
}

/// The Markdown startup block, ending with one newline.
pub fn build_startup_info(input: &StartupInfoInput<'_>) -> String {
    let mut md: Vec<String> = vec![format!("pi v{}", input.version), "---".into(), String::new()];
    let mut section = |title: &str, items: Vec<String>| {
        let items: Vec<String> =
            items.iter().map(|item| item.trim().to_string()).filter(|item| !item.is_empty()).collect();
        if items.is_empty() {
            return;
        }
        md.push(format!("## {title}"));
        md.extend(items.into_iter().map(|item| format!("- {item}")));
        md.push(String::new());
    };
    let context = input.cwd.join("AGENTS.md");
    section("Context", context.exists().then(|| context.display().to_string()).into_iter().collect());

    let mut skills = Vec::new();
    skills_from_root(&input.agent_dir.join("skills"), &mut skills);
    skills_from_root(&input.home.join(".agents").join("skills"), &mut skills);
    skills_from_root(&input.cwd.join(".pi").join("skills"), &mut skills);
    section("Skills", skills);

    let prompts = sorted_entries(&input.agent_dir.join("prompts"))
        .into_iter()
        .filter_map(|entry| entry.file_name().to_string_lossy().strip_suffix(".md").map(|name| format!("/{name}")))
        .collect();
    section("Prompts", prompts);

    let extensions_dir = input.agent_dir.join("extensions");
    let mut extensions: Vec<String> = sorted_entries(&extensions_dir)
        .into_iter()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.ends_with(".ts") || name.ends_with(".js")
        })
        .map(|entry| entry.path().display().to_string())
        .collect();
    extensions.extend(settings_packages(&input.agent_dir.join("settings.json")));
    extensions.extend(settings_packages(&input.cwd.join(".pi").join("settings.json")));
    section("Extensions", extensions);

    if let Some(notice) = input.update_notice {
        md.push("---".into());
        md.push(notice.to_string());
        md.push(String::new());
    }
    md.join("\n").trim().to_string() + "\n"
}
