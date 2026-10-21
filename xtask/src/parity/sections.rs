//! Sections 2 to 5, 7, and 9: surface lists read from source text on both sides.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use super::parse::{const_array_entries, identifiers, long_flags, serde_fields, ts_members};
use super::report::{Item, Section};
use super::scan::Lang;

const CODING_AGENT: &str = "packages/coding-agent/src";
const RUST_AGENT: &str = "crates/pi-coding-agent/src";

/// Run `fill` on a fresh section; an error becomes a skipped section with that reason.
pub fn build(id: &'static str, title: &'static str, fill: impl FnOnce(&mut Section) -> Result<(), String>) -> Section {
    let mut section = Section::new(id, title);
    match fill(&mut section) {
        Ok(()) => section,
        Err(reason) => Section::skipped(id, title, reason),
    }
}

pub fn read_upstream(upstream: &Path, rel: &str) -> Result<String, String> {
    std::fs::read_to_string(upstream.join(rel)).map_err(|_| format!("upstream file not found: {rel}"))
}

pub fn read_local(root: &Path, rel: &str) -> Result<String, String> {
    std::fs::read_to_string(root.join(rel)).map_err(|error| format!("{rel}: {error}"))
}

/// Items only upstream, then items only in pi-rust.
pub fn diff(upstream: &BTreeSet<String>, local: &BTreeSet<String>) -> (Vec<Item>, Vec<Item>) {
    let only = |a: &BTreeSet<String>, b: &BTreeSet<String>| -> Vec<Item> {
        a.difference(b).map(|name| Item::new(name.as_str())).collect()
    };
    (only(upstream, local), only(local, upstream))
}

fn set(names: Vec<String>) -> BTreeSet<String> {
    names.into_iter().collect()
}

fn compare(section: &mut Section, upstream: &BTreeSet<String>, local: &BTreeSet<String>, what: &str) {
    section.notes.push(format!("Upstream has {} {what}; pi-rust has {}.", upstream.len(), local.len()));
    (section.missing, section.extra) = diff(upstream, local);
}

/// Every file with one of `extensions` under `dir`, recursively and in path order.
pub fn files_under(dir: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| extensions.contains(&ext)) {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

pub fn cli_flags(upstream: &Path, root: &Path) -> Section {
    build("cliFlags", "CLI flags", |section| {
        let up = long_flags(&read_upstream(upstream, &format!("{CODING_AGENT}/cli/args.ts"))?);
        let parser = read_local(root, &format!("{RUST_AGENT}/cli.rs"))?;
        let help = read_local(root, &format!("{RUST_AGENT}/cli_help.rs"))?;
        compare(section, &up, &long_flags(&format!("{parser}\n{help}")), "long flags");
        Ok(())
    })
}

pub fn settings(upstream: &Path, root: &Path) -> Section {
    build("settings", "Settings keys", |section| {
        let source = read_upstream(upstream, &format!("{CODING_AGENT}/core/settings-schema.ts"))?;
        let anchors = ["export const SettingsSchema", "export interface Settings", "export type Settings"];
        let (anchor, keys) =
            ts_members(&source, &anchors).ok_or("no Settings schema or interface found in settings-schema.ts")?;
        let rust = read_local(root, &format!("{RUST_AGENT}/settings/schema.rs"))?;
        let local = serde_fields(&rust, "Settings").ok_or("no `pub struct Settings` in settings/schema.rs")?;
        section.notes.push(format!("Upstream keys come from `{anchor}`."));
        let (mut keys, mut local) = (set(keys), set(local));
        // pi-rust adds its own `terminal.*` keys, so compare that object's members too.
        let terminal =
            ts_members(&source, &["const TerminalSettingsSchema"]).zip(serde_fields(&rust, "TerminalSettings"));
        if let Some(((_, upstream_terminal), local_terminal)) = terminal {
            keys.extend(upstream_terminal.iter().map(|key| format!("terminal.{key}")));
            local.extend(local_terminal.iter().map(|key| format!("terminal.{key}")));
        }
        compare(section, &keys, &local, "settings keys (top level and `terminal.*`)");
        Ok(())
    })
}

pub fn slash_commands(upstream: &Path, root: &Path) -> Section {
    build("slashCommands", "Slash commands", |section| {
        let source = read_upstream(upstream, &format!("{CODING_AGENT}/core/slash-commands.ts"))?;
        let up = const_array_entries(&source, "BUILTIN_SLASH_COMMANDS", Lang::Ts)
            .ok_or("no BUILTIN_SLASH_COMMANDS upstream")?;
        let dir = root.join(RUST_AGENT).join("interactive");
        let local = files_under(&dir, &["rs"])
            .iter()
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .find_map(|text| const_array_entries(&text, "BUILTIN_SLASH_COMMANDS", Lang::Rust))
            .ok_or("no BUILTIN_SLASH_COMMANDS under crates/pi-coding-agent/src/interactive")?;
        compare(section, &set(up), &set(local), "built-in slash commands");
        Ok(())
    })
}

/// File stems in `dir` whose text defines a tool of that name.
fn tool_stems(dir: &Path, extension: &str, defines: impl Fn(&str, &str) -> bool) -> Result<BTreeSet<String>, String> {
    let files = files_under(dir, &[extension]);
    if files.is_empty() {
        return Err(format!("no .{extension} files in {}", dir.display()));
    }
    let stems = files.iter().filter(|path| path.parent() == Some(dir)).filter_map(|path| {
        let stem = path.file_stem()?.to_str()?.to_string();
        let text = std::fs::read_to_string(path).ok()?;
        defines(&stem, &text).then_some(stem)
    });
    Ok(stems.collect())
}

pub fn tools(upstream: &Path, root: &Path) -> Section {
    build("tools", "Built-in tools", |section| {
        let dir = upstream.join(CODING_AGENT).join("core/tools");
        if !dir.is_dir() {
            return Err(format!("upstream directory not found: {CODING_AGENT}/core/tools"));
        }
        let up = tool_stems(&dir, "ts", |stem, text| text.contains(&format!("name: \"{stem}\"")))?;
        let local_dir = root.join(RUST_AGENT).join("tools");
        let local = tool_stems(&local_dir, "rs", |stem, text| {
            text.contains(&format!("\"{stem}\""))
                && (text.contains("AgentTool") || text.contains(&format!("name: \"{stem}\"")))
        })?;
        compare(section, &up, &local, "tool files");
        Ok(())
    })
}

pub fn extension_api(upstream: &Path, root: &Path) -> Section {
    build("extensionApi", "Extension API", |section| {
        let source = read_upstream(upstream, &format!("{CODING_AGENT}/core/extensions/types.ts"))?;
        let (_, names) = ts_members(&source, &["export interface ExtensionAPI"])
            .ok_or("no `export interface ExtensionAPI` in types.ts")?;
        let mut seen = HashSet::new();
        let files = files_under(&root.join(RUST_AGENT).join("extensions"), &["rs", "js", "mjs", "cjs", "ts"]);
        for text in files.iter().filter_map(|path| std::fs::read_to_string(path).ok()) {
            identifiers(&text, &mut seen);
        }
        let names = set(names);
        section.notes.push(format!(
            "ExtensionAPI declares {} members; {} files under `{RUST_AGENT}/extensions` were searched for each name. Only members with no match are listed.",
            names.len(),
            files.len()
        ));
        section.missing =
            names.iter().filter(|name| !seen.contains(*name)).map(|name| Item::new(name.as_str())).collect();
        Ok(())
    })
}

fn child_dirs(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.') && name != "node_modules")
        .collect();
    names.sort();
    names
}

pub fn packages(upstream: &Path, root: &Path) -> Section {
    build("packages", "Packages", |section| {
        let up = child_dirs(&upstream.join("packages"));
        if up.is_empty() {
            return Err("upstream directory not found or empty: packages".into());
        }
        let crates = child_dirs(&root.join("crates"));
        let local: BTreeSet<String> = crates.iter().filter(|name| name.starts_with("pi-")).cloned().collect();
        let mapped: BTreeSet<String> = up.iter().map(|name| format!("pi-{name}")).collect();
        section.notes.push(format!(
            "Upstream package `x` maps to crate `pi-x`. Upstream has {} packages; pi-rust has {} crates.",
            up.len(),
            local.len()
        ));
        section.missing = up
            .iter()
            .filter(|name| !local.contains(&format!("pi-{name}")))
            .map(|name| Item::new(name.as_str()))
            .collect();
        section.extra = local.difference(&mapped).map(|name| Item::new(name.as_str())).collect();
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_splits_both_sides() {
        let a: BTreeSet<String> = ["x", "y"].map(String::from).into();
        let b: BTreeSet<String> = ["y", "z"].map(String::from).into();
        let (missing, extra) = diff(&a, &b);
        assert_eq!((missing[0].name.as_str(), extra[0].name.as_str()), ("x", "z"));
    }

    #[test]
    fn tool_files_are_told_apart_from_helpers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bash.ts"), "export const t = { name: \"bash\", x: 1 };").unwrap();
        std::fs::write(dir.path().join("truncate.ts"), "export const limit = 1;").unwrap();
        let stems = tool_stems(dir.path(), "ts", |stem, text| text.contains(&format!("name: \"{stem}\""))).unwrap();
        assert_eq!(stems, BTreeSet::from(["bash".to_string()]));
        assert!(tool_stems(&dir.path().join("none"), "ts", |_, _| true).is_err());
    }

    #[test]
    fn missing_upstream_files_skip_the_section() {
        let dir = tempfile::tempdir().unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        for section in [cli_flags(dir.path(), root), settings(dir.path(), root), slash_commands(dir.path(), root)] {
            assert!(
                matches!(&section.status, super::super::report::Status::Skipped(reason) if reason.contains("not found")),
                "{}",
                section.id
            );
        }
        assert!(matches!(tools(dir.path(), root).status, super::super::report::Status::Skipped(_)));
        assert!(matches!(packages(dir.path(), root).status, super::super::report::Status::Skipped(_)));
    }
}
