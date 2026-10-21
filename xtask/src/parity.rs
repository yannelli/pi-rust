//! `cargo xtask parity --upstream <path> [--out <file>] [--strict]`: a Markdown gap report that
//! compares an upstream pi checkout with this repository. It is a heuristic text comparison:
//! no network, no AI provider, and only `git`, `node` (optional), and the files on disk.

mod catalog;
mod commits;
mod node;
mod parse;
mod report;
mod scan;
mod sections;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use report::{Allowlist, Report};
use serde_json::Value;

/// Where this repository last synced with upstream, from `docs/parity/upstream.json`.
#[derive(Debug)]
pub struct Synced {
    pub repository: String,
    pub branch: String,
    pub commit: String,
    pub version: String,
    pub date: String,
}

impl Synced {
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = root.join("docs/parity/upstream.json");
        let text = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let value: Value = serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        let field = |name: &str| {
            let text = value.get(name).and_then(Value::as_str);
            text.map(String::from).ok_or_else(|| format!("{}: missing string field {name:?}", path.display()))
        };
        Ok(Self {
            repository: field("repository")?,
            branch: field("branch")?,
            commit: field("syncedCommit")?,
            version: field("syncedVersion")?,
            date: field("syncedOn")?,
        })
    }
}

struct Options {
    upstream: PathBuf,
    out: Option<PathBuf>,
    strict: bool,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let (mut upstream, mut out, mut strict) = (None, None, false);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--upstream" => upstream = Some(PathBuf::from(rest.next().ok_or("--upstream needs a path")?)),
            "--out" => out = Some(PathBuf::from(rest.next().ok_or("--out needs a path")?)),
            "--strict" => strict = true,
            other => return Err(format!("parity: unexpected argument {other}")),
        }
    }
    let upstream = upstream.ok_or("parity needs --upstream <path to a checkout of earendil-works/pi>")?;
    Ok(Options { upstream, out, strict })
}

fn package_version(upstream: &Path) -> Option<String> {
    let text = std::fs::read_to_string(upstream.join("packages/coding-agent/package.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get("version")?.as_str().map(String::from)
}

fn header(upstream: &Path, synced: &Synced) -> Vec<String> {
    let url = format!("https://github.com/{}", synced.repository);
    let head = match commits::head(upstream) {
        Some((sha, date)) => format!("Upstream HEAD: [`{sha}`]({url}/commit/{sha}) ({date})"),
        None => "Upstream HEAD: unknown (the path is not a git checkout)".to_string(),
    };
    let mut lines = vec![format!("Upstream: [{}]({url}), branch `{}`", synced.repository, synced.branch), head];
    if let Some(version) = package_version(upstream) {
        lines.push(format!("Upstream coding-agent version: {version}"));
    }
    let short = synced.commit.get(..9).unwrap_or(&synced.commit);
    lines.push(format!("Last sync: `{short}`, version {}, on {}", synced.version, synced.date));
    lines
}

/// Run every section against `upstream` and apply the allowlist.
pub fn build_report(upstream: &Path, root: &Path, synced: &Synced, allowlist: Allowlist) -> Report {
    let sections = vec![
        commits::section(upstream, root, &synced.commit, &synced.repository),
        sections::cli_flags(upstream, root),
        sections::settings(upstream, root),
        sections::slash_commands(upstream, root),
        sections::tools(upstream, root),
        catalog::section(upstream, root),
        sections::extension_api(upstream, root),
        node::section(root),
        sections::packages(upstream, root),
    ];
    Report::new(header(upstream, synced), sections, allowlist)
}

pub fn run(args: &[String], root: &Path) -> Result<(), String> {
    let options = parse_args(args)?;
    if !options.upstream.is_dir() {
        return Err(format!("parity: {} is not a directory", options.upstream.display()));
    }
    let synced = Synced::load(root)?;
    let allowlist = Allowlist::load(&root.join("docs/parity/allowlist.json"))?;
    let report = build_report(&options.upstream, root, &synced, allowlist);
    let text = report.render();
    let findings = report.findings();
    match &options.out {
        Some(out) => {
            if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
            }
            std::fs::write(out, &text).map_err(|error| format!("{}: {error}", out.display()))?;
            println!("Wrote {} ({findings} findings not in the allowlist)", out.display());
        }
        None => print!("{text}"),
    }
    if options.strict && findings > 0 {
        return Err(format!("{findings} parity findings are not in docs/parity/allowlist.json"));
    }
    Ok(())
}
