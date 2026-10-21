//! Release tooling: `cargo xtask plan | prepare | notes | check-tag`. Versions come from
//! conventional commits since the last `v*` tag; see `usage()` for arguments.

mod models;
mod parity;
mod vendor_js;
mod version;

use std::path::{Path, PathBuf};
use std::process::Command;

use version::{Bump, Change, Commit, Section, Version, classify, required_bump};

const CHANGELOG_HEADER: &str = "# Changelog\n\nAll notable changes to pi-rust are documented here. Versions follow [Semantic Versioning](https://semver.org/); before 1.0, breaking changes bump the minor version.\n";
const REPOSITORY: &str = "https://github.com/yannelli/pi-rust";

fn usage() -> String {
    "usage: cargo xtask <command>
  plan [--bump auto|patch|minor|major] [--github-output]   next version from conventional commits
  prepare <version>                                        set the version, refresh Cargo.lock, update CHANGELOG.md
  notes <version>                                          print the CHANGELOG.md section for a version
  check-tag <tag>                                          fail unless the tag matches the workspace version
  models <upstream-data-dir> [--out <path>]                regenerate crates/pi-ai/models.json from upstream
  vendor-js <upstream-root> [--out <path>]                 bundle the upstream JavaScript the extension host embeds
  parity --upstream <path> [--out <file>] [--strict]       write a Markdown gap report against an upstream pi checkout"
        .into()
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask lives in the workspace").to_path_buf()
}

fn git(args: &[&str]) -> Result<String, String> {
    let output =
        Command::new("git").args(args).current_dir(root()).output().map_err(|error| format!("git: {error}"))?;
    if !output.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn workspace_version() -> Result<Version, String> {
    let manifest = std::fs::read_to_string(root().join("Cargo.toml")).map_err(|error| error.to_string())?;
    let mut in_package = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[workspace.package]";
        } else if in_package && let Some(value) = trimmed.strip_prefix("version") {
            let value = value.trim_start().trim_start_matches('=').trim().trim_matches('"');
            return value.parse();
        }
    }
    Err("no version in [workspace.package]".into())
}

fn set_workspace_version(version: &Version) -> Result<(), String> {
    let path = root().join("Cargo.toml");
    let manifest = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let mut in_package = false;
    let mut replaced = false;
    let lines: Vec<String> = manifest
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_package = trimmed == "[workspace.package]";
            } else if in_package && !replaced && trimmed.starts_with("version") && trimmed.contains('=') {
                replaced = true;
                return format!("version = \"{version}\"");
            }
            line.to_string()
        })
        .collect();
    if !replaced {
        return Err("no version in [workspace.package]".into());
    }
    std::fs::write(&path, lines.join("\n") + "\n").map_err(|error| error.to_string())
}

/// The nearest `v*` tag reachable from HEAD.
fn last_tag() -> Option<String> {
    git(&["describe", "--tags", "--abbrev=0", "--match", "v[0-9]*"]).ok().map(|tag| tag.trim().to_string())
}

fn tag_exists(tag: &str) -> bool {
    git(&["rev-parse", "--verify", "--quiet", &format!("refs/tags/{tag}")]).is_ok()
}

fn commits_since(tag: Option<&str>) -> Result<Vec<Commit>, String> {
    let range = tag.map(|tag| format!("{tag}..HEAD")).unwrap_or_else(|| "HEAD".into());
    let log = git(&["log", "--no-merges", "--format=%H%x1f%s%x1f%b%x1e", &range])?;
    Ok(log
        .split('\x1e')
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').splitn(3, '\x1f');
            let hash = fields.next()?.trim().to_string();
            let subject = fields.next()?.trim().to_string();
            let body = fields.next().unwrap_or_default().trim().to_string();
            (!hash.is_empty()).then_some(Commit { hash, subject, body })
        })
        .collect())
}

struct Plan {
    version: Option<Version>,
    changes: Vec<Change>,
}

fn plan(bump: Option<Bump>) -> Result<Plan, String> {
    let current = workspace_version()?;
    let changes: Vec<Change> = commits_since(last_tag().as_deref())?.iter().filter_map(classify).collect();
    // A version that was never tagged is released as-is, for example the first release.
    if !tag_exists(&format!("v{current}")) {
        return Ok(Plan { version: Some(current), changes });
    }
    let version = bump.or_else(|| required_bump(&changes)).map(|bump| current.bump(bump));
    Ok(Plan { version, changes })
}

fn render_section(version: &Version, date: &str, changes: &[Change]) -> String {
    let mut text = format!("## [{version}] - {date}\n");
    for section in [Section::Breaking, Section::Added, Section::Changed, Section::Fixed] {
        let entries: Vec<String> = changes
            .iter()
            .filter(|change| change.section == section)
            .map(|change| {
                let scope = change.scope.as_deref().map(|scope| format!("**{scope}:** ")).unwrap_or_default();
                let short = &change.hash[..change.hash.len().min(7)];
                format!("- {scope}{} ([{short}]({REPOSITORY}/commit/{}))", change.description, change.hash)
            })
            .collect();
        if !entries.is_empty() {
            text.push_str(&format!("\n### {}\n\n{}\n", section.title(), entries.join("\n")));
        }
    }
    if changes.is_empty() {
        text.push_str("\nMaintenance release.\n");
    }
    text
}

fn insert_section(changelog: &str, section: &str) -> String {
    let changelog = if changelog.trim().is_empty() { CHANGELOG_HEADER.to_string() } else { changelog.to_string() };
    match changelog.find("\n## ") {
        Some(index) => format!("{}\n{}\n{}", &changelog[..index], section.trim_end(), &changelog[index..]),
        None => format!("{}\n{}", changelog.trim_end_matches('\n').to_string() + "\n", section),
    }
}

/// The body of `version`'s changelog section, without its heading.
fn section_notes(changelog: &str, version: &Version) -> Option<String> {
    let heading = format!("## [{version}]");
    let start = changelog.find(&heading)?;
    let body_start = start + changelog[start..].find('\n')? + 1;
    let end = changelog[body_start..].find("\n## ").map(|offset| body_start + offset).unwrap_or(changelog.len());
    Some(changelog[body_start..end].trim().to_string())
}

fn write_github_output(pairs: &[(&str, String)]) -> Result<(), String> {
    use std::io::Write;
    let path = std::env::var("GITHUB_OUTPUT").map_err(|_| "GITHUB_OUTPUT is not set".to_string())?;
    let text: String = pairs.iter().map(|(key, value)| format!("{key}={value}\n")).collect();
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|error| error.to_string())
}

fn run(args: &[String]) -> Result<(), String> {
    let flag_value =
        |name: &str| args.iter().position(|arg| arg == name).and_then(|index| args.get(index + 1)).cloned();
    match args.first().map(String::as_str) {
        Some("plan") => {
            let bump = match flag_value("--bump").as_deref() {
                None | Some("auto") => None,
                Some(bump) => Some(bump.parse()?),
            };
            let plan = plan(bump)?;
            let version = plan.version.map(|version| version.to_string()).unwrap_or_default();
            let pairs = [
                ("release", (!version.is_empty()).to_string()),
                ("tag", if version.is_empty() { String::new() } else { format!("v{version}") }),
                ("prerelease", version.contains('-').to_string()),
                ("version", version),
                ("changes", plan.changes.len().to_string()),
            ];
            for (key, value) in &pairs {
                println!("{key}={value}");
            }
            if args.iter().any(|arg| arg == "--github-output") {
                write_github_output(&pairs)?;
            }
            Ok(())
        }
        Some("prepare") => {
            let version: Version = args.get(1).ok_or("prepare needs a version")?.parse()?;
            let changes: Vec<Change> = commits_since(last_tag().as_deref())?.iter().filter_map(classify).collect();
            if version != workspace_version()? {
                set_workspace_version(&version)?;
            }
            let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
                .args(["update", "--workspace", "--offline"])
                .current_dir(root())
                .status()
                .map_err(|error| error.to_string())?;
            if !status.success() {
                return Err("cargo update --workspace failed".into());
            }
            let path = root().join("CHANGELOG.md");
            let changelog = std::fs::read_to_string(&path).unwrap_or_default();
            if section_notes(&changelog, &version).is_none() {
                let date = git(&["log", "-1", "--format=%cs", "HEAD"])?.trim().to_string();
                let updated = insert_section(&changelog, &render_section(&version, &date, &changes));
                std::fs::write(&path, updated).map_err(|error| error.to_string())?;
            }
            println!("Prepared v{version}");
            Ok(())
        }
        Some("notes") => {
            let version: Version = args.get(1).ok_or("notes needs a version")?.parse()?;
            let changelog = std::fs::read_to_string(root().join("CHANGELOG.md")).unwrap_or_default();
            let notes =
                section_notes(&changelog, &version).ok_or(format!("CHANGELOG.md has no section for {version}"))?;
            println!("{notes}");
            Ok(())
        }
        Some("check-tag") => {
            let tag = args.get(1).ok_or("check-tag needs a tag")?;
            let tagged: Version = tag.parse()?;
            let current = workspace_version()?;
            if !tag.starts_with('v') || tagged != current {
                return Err(format!("tag {tag} does not match workspace version {current}"));
            }
            println!("Tag {tag} matches the workspace version");
            Ok(())
        }
        Some("models") => models::run(&args[1..], &root()),
        Some("vendor-js") => vendor_js::run(&args[1..], &root()),
        Some("parity") => parity::run(&args[1..], &root()),
        _ => Err(usage()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = run(&args) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(section: Section, scope: Option<&str>, description: &str, hash: &str) -> Change {
        Change { section, scope: scope.map(String::from), description: description.into(), hash: hash.into() }
    }

    #[test]
    fn changelog_sections_insert_and_extract() {
        let changes = vec![
            change(Section::Added, Some("tui"), "picker", "abcdef1234"),
            change(Section::Fixed, None, "crash", "1234567890"),
        ];
        let first: Version = "0.1.0".parse().unwrap();
        let second: Version = "0.1.1".parse().unwrap();
        let changelog = insert_section("", &render_section(&first, "2026-10-12", &changes[..1]));
        let changelog = insert_section(&changelog, &render_section(&second, "2026-10-13", &changes));
        assert!(changelog.starts_with(CHANGELOG_HEADER));
        assert!(changelog.find("## [0.1.1]").unwrap() < changelog.find("## [0.1.0]").unwrap());
        let notes = section_notes(&changelog, &second).unwrap();
        assert!(notes.contains("### Added\n\n- **tui:** picker ([abcdef1]"));
        assert!(notes.contains("### Fixed\n\n- crash"));
        assert!(!notes.contains("0.1.0"));
        assert!(section_notes(&changelog, &first).unwrap().contains("picker"));
    }
}
