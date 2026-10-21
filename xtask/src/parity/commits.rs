//! Section 1: upstream commits after the synced commit that this repository has not ported.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use super::report::{Item, Section};

const ID: &str = "commits";
const TITLE: &str = "Upstream commits";

/// Conventional commit types that never need a port on their own.
const QUIET_TYPES: &[&str] = &["docs", "test", "tests", "chore", "ci", "style"];

#[derive(Debug, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub date: String,
    pub subject: String,
    pub files: Vec<String>,
}

pub fn git_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|error| format!("git: {error}"))?;
    if !output.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse `git log --name-only --format=%x1e%H%x09%cs%x09%s` output.
pub fn parse_log(text: &str) -> Vec<Commit> {
    text.split('\x1e')
        .filter_map(|record| {
            let mut lines = record.lines();
            let mut fields = lines.next()?.splitn(3, '\t');
            let (sha, date, subject) = (fields.next()?, fields.next()?, fields.next()?);
            let files = lines.map(str::trim).filter(|line| !line.is_empty()).map(String::from).collect();
            Some(Commit { sha: sha.into(), date: date.into(), subject: subject.into(), files })
        })
        .collect()
}

/// The lower-case type of a conventional commit subject, or an empty string.
pub fn commit_type(subject: &str) -> String {
    let word: String = subject.chars().take_while(char::is_ascii_alphabetic).collect();
    let next = subject[word.len()..].chars().next();
    if matches!(next, Some('(' | '!' | ':')) { word.to_lowercase() } else { String::new() }
}

fn kind(subject: &str) -> &'static str {
    match commit_type(subject).as_str() {
        "feat" => "feat",
        "fix" => "fix",
        _ => "other",
    }
}

fn is_doc_or_test_file(file: &str) -> bool {
    file.ends_with(".md") || ["/test/", "/tests/", ".test.", ".spec.", "/docs/"].iter().any(|part| file.contains(part))
}

/// Docs, test, chore, and release commits, or commits that touch only docs and test files.
pub fn is_quiet(commit: &Commit) -> bool {
    let subject = commit.subject.as_str();
    QUIET_TYPES.contains(&commit_type(subject).as_str())
        || subject.starts_with("Release v")
        || subject.starts_with("Add [Unreleased]")
        || (!commit.files.is_empty() && commit.files.iter().all(|file| is_doc_or_test_file(file)))
}

fn packages(commit: &Commit) -> BTreeSet<&str> {
    commit.files.iter().filter_map(|file| file.strip_prefix("packages")?.strip_prefix('/')?.split('/').next()).collect()
}

fn short(sha: &str) -> &str {
    sha.get(..9).unwrap_or(sha)
}

pub struct Triage {
    pub unported: Vec<Item>,
    pub quiet: Vec<String>,
    pub ported: usize,
}

/// Split commits into ported (their short SHA is in `ported_text`), quiet, and unported.
pub fn triage(commits: &[Commit], ported_text: &str, repository: &str) -> Triage {
    let mut result = Triage { unported: Vec::new(), quiet: Vec::new(), ported: 0 };
    for commit in commits {
        let sha = short(&commit.sha);
        if ported_text.contains(sha) {
            result.ported += 1;
        } else if is_quiet(commit) {
            result.quiet.push(format!("- `{sha}` {} {}", commit.date, commit.subject.replace('<', "&lt;")));
        } else {
            for package in packages(commit) {
                result.unported.push(Item {
                    name: sha.to_string(),
                    detail: format!("{} {}", commit.date, commit.subject),
                    group: Some(format!("{package} / {}", kind(&commit.subject))),
                    link: Some(format!("https://github.com/{repository}/commit/{}", commit.sha)),
                });
            }
        }
    }
    result
}

/// The upstream HEAD commit and its date, when `upstream` is a git checkout.
pub fn head(upstream: &Path) -> Option<(String, String)> {
    let line = git_in(upstream, &["log", "-1", "--format=%H%x09%cs"]).ok()?;
    let (sha, date) = line.trim().split_once('\t')?;
    Some((sha.into(), date.into()))
}

pub fn section(upstream: &Path, root: &Path, synced: &str, repository: &str) -> Section {
    let skipped = |reason: String| Section::skipped(ID, TITLE, reason);
    let toplevel = git_in(upstream, &["rev-parse", "--show-toplevel"]).ok();
    let is_root = toplevel.as_deref().map(|top| Path::new(top.trim())).and_then(|top| top.canonicalize().ok())
        == upstream.canonicalize().ok();
    if !is_root {
        return skipped("the upstream path is not the root of a git checkout".into());
    }
    if git_in(upstream, &["cat-file", "-e", &format!("{synced}^{{commit}}")]).is_err() {
        return skipped(format!(
            "syncedCommit {} is not in the upstream checkout (older snapshot or shallow clone)",
            short(synced)
        ));
    }
    let range = format!("{synced}..HEAD");
    let format = "--format=%x1e%H%x09%cs%x09%s";
    let log = match git_in(upstream, &["log", "--no-merges", "--name-only", format, &range, "--", "packages"]) {
        Ok(log) => log,
        Err(error) => return skipped(error),
    };
    let Ok(ported_text) = git_in(root, &["log", "--format=%B"]) else {
        return skipped("cannot read the git log of this repository".into());
    };
    let commits = parse_log(&log);
    let triage = triage(&commits, &ported_text, repository);
    let mut section = Section::new(ID, TITLE);
    let unported: BTreeSet<&str> = triage.unported.iter().map(|item| item.name.as_str()).collect();
    section.notes.push(format!(
        "{} upstream commits after `{}`: {} ported, {} not ported, {} docs, test, or chore only.",
        commits.len(),
        short(synced),
        triage.ported,
        unported.len(),
        triage.quiet.len()
    ));
    if git_in(root, &["rev-parse", "--is-shallow-repository"]).is_ok_and(|shallow| shallow.trim() == "true") {
        section.notes.push("This repository is a shallow clone, so ported commits may be reported as missing.".into());
    }
    if !triage.quiet.is_empty() {
        section.collapsed =
            Some((format!("{} docs, test, or chore only commits skipped", triage.quiet.len()), triage.quiet));
    }
    section.missing = triage.unported;
    section
}

#[cfg(test)]
mod tests {
    use super::super::report::Status;
    use super::*;

    const LOG: &str = "\x1eaaaaaaaaa1111\t2026-10-12\tfeat(tui,ai): add a thing\n\npackages/tui/src/a.ts\npackages/ai/src/b.ts\n\
\x1ebbbbbbbbb2222\t2026-10-11\tfix(ai): handle <tag>\n\npackages/ai/src/c.ts\n\
\x1eccccccccc3333\t2026-10-10\tdocs: update\n\npackages/ai/README.md\n\
\x1eddddddddd4444\t2026-10-09\tRelease v1.2.0\n\npackages/ai/package.json\n\
\x1eeeeeeeee5555\t2026-10-08\tfix: only tests\n\npackages/ai/test/x.test.ts\n\
\x1efffffffff6666\t2026-10-07\trefactor(agent): move code\n\npackages/agent/src/d.ts\n";

    #[test]
    fn parses_log_records() {
        let commits = parse_log(LOG);
        assert_eq!(commits.len(), 6);
        assert_eq!(commits[0].sha, "aaaaaaaaa1111");
        assert_eq!(commits[0].date, "2026-10-12");
        assert_eq!(commits[0].subject, "feat(tui,ai): add a thing");
        assert_eq!(commits[0].files, ["packages/tui/src/a.ts", "packages/ai/src/b.ts"]);
        assert!(parse_log("").is_empty());
    }

    #[test]
    fn commit_types() {
        assert_eq!(commit_type("feat(tui)!: x"), "feat");
        assert_eq!(commit_type("Fix: x"), "fix");
        assert_eq!(commit_type("Release v1"), "");
        assert_eq!(commit_type("fixing things"), "");
        assert_eq!(kind("perf: x"), "other");
    }

    #[test]
    fn triage_groups_by_package_and_kind() {
        let commits = parse_log(LOG);
        let result = triage(&commits, "Port of upstream bbbbbbbbb", "earendil-works/pi");
        assert_eq!(result.ported, 1);
        assert_eq!(result.quiet.len(), 3);
        let groups: Vec<_> =
            result.unported.iter().map(|item| (item.name.as_str(), item.group.clone().unwrap())).collect();
        assert_eq!(
            groups,
            [
                ("aaaaaaaaa", "ai / feat".to_string()),
                ("aaaaaaaaa", "tui / feat".to_string()),
                ("fffffffff", "agent / other".to_string()),
            ]
        );
        assert_eq!(
            result.unported[0].link.as_deref(),
            Some("https://github.com/earendil-works/pi/commit/aaaaaaaaa1111")
        );
    }

    fn git_commit(dir: &Path, subject: &str) -> String {
        let config = ["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"];
        let run = |args: &[&str]| git_in(dir, &[&config[..], args].concat()).unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "--allow-empty", "-m", subject]);
        run(&["rev-parse", "HEAD"]).trim().to_string()
    }

    #[test]
    fn real_git_checkout_end_to_end() {
        if git_in(Path::new("."), &["--version"]).is_err() {
            return;
        }
        let (upstream, ours) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (up, root) = (upstream.path(), ours.path());
        git_in(up, &["init", "-q"]).unwrap();
        git_in(root, &["init", "-q"]).unwrap();
        std::fs::create_dir_all(up.join("packages/ai")).unwrap();
        let mut shas = Vec::new();
        for (index, subject) in ["feat(ai): base", "feat(ai): ported one", "fix(ai): later"].iter().enumerate() {
            std::fs::write(up.join(format!("packages/ai/f{index}.ts")), "x").unwrap();
            shas.push(git_commit(up, subject));
        }
        git_commit(root, &format!("feat: port upstream {}", &shas[1][..9]));
        let found = section(up, root, &shas[0], "o/r");
        assert_eq!(found.missing.len(), 1, "{found:?}");
        assert_eq!(found.missing[0].group.as_deref(), Some("ai / fix"));
        assert!(found.notes[0].contains("1 ported, 1 not ported"), "{:?}", found.notes);
        let unknown = section(up, root, &"0".repeat(40), "o/r");
        assert!(matches!(unknown.status, Status::Skipped(_)));
        let plain = tempfile::tempdir().unwrap();
        assert!(matches!(section(plain.path(), root, &shas[0], "o/r").status, Status::Skipped(_)));
    }
}
