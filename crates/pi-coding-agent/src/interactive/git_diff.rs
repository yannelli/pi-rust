//! `/diff` (pi-rust extension): the uncommitted git changes in the session cwd. It shows the
//! `git diff HEAD` stat, the untracked files, and the diff, colored like tool diffs.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use pi_tui::component;

use super::mode::InteractiveMode;
use crate::interactive_components::DynamicBorder;
use crate::platform::shell::sanitize_binary_output;
use crate::theme::{ThemeColor, theme};

/// Lines shown in the chat before the rest is cut.
const MAX_DIFF_LINES: usize = 500;
const GIT_TIMEOUT: Duration = Duration::from_secs(15);

/// `/diff [--stat] [path]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DiffArgs {
    stat_only: bool,
    path: Option<String>,
}

/// `--stat` before or after the path; the path may be quoted.
fn parse_diff_args(text: Option<&str>) -> DiffArgs {
    let mut rest = text.unwrap_or("").trim();
    let mut stat_only = false;
    let word_end = |after: &str| after.is_empty() || after.starts_with(char::is_whitespace);
    if let Some(after) = rest.strip_prefix("--stat").filter(|after| word_end(after)) {
        stat_only = true;
        rest = after.trim_start();
    } else if let Some(before) = rest.strip_suffix("--stat").filter(|before| before.ends_with(char::is_whitespace)) {
        stat_only = true;
        rest = before.trim_end();
    }
    let unquoted = ['"', '\''].iter().find_map(|quote| rest.strip_prefix(*quote)?.strip_suffix(*quote));
    let path = unquoted.unwrap_or(rest);
    DiffArgs { stat_only, path: (!path.is_empty()).then(|| path.to_string()) }
}

/// The output of the git commands; empty strings when there is nothing to show.
#[derive(Debug, Default)]
struct GitChanges {
    stat: String,
    untracked: String,
    diff: String,
}

impl GitChanges {
    fn is_empty(&self) -> bool {
        [&self.stat, &self.untracked, &self.diff].iter().all(|text| text.trim().is_empty())
    }
}

/// What `/diff` shows; the lines of a report are built off the UI thread.
#[derive(Debug)]
enum Collected {
    GitMissing,
    NotRepository,
    Failed(String),
    Clean,
    Changes(Vec<(LineKind, String)>),
}

fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("").to_string()
}

/// `git <args>` in `cwd`, without a shell and without taking optional locks.
async fn git(cwd: &Path, args: &[&str]) -> Result<Output, Collected> {
    let mut command = tokio::process::Command::new("git");
    command.arg("--no-optional-locks").args(args).current_dir(cwd);
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    match tokio::time::timeout(GIT_TIMEOUT, command.output()).await {
        Err(_) => Err(Collected::Failed(format!("git {} timed out", args[0]))),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Err(Collected::GitMissing),
        Ok(Err(error)) => Err(Collected::Failed(format!("Could not run git: {error}"))),
        Ok(Ok(output)) => Ok(output),
    }
}

/// The standard output of a git command that must succeed.
async fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String, Collected> {
    let output = git(cwd, args).await?;
    if !output.status.success() {
        return Err(Collected::Failed(format!("git {} failed: {}", args[0], first_line(&output.stderr))));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `head`, then `base`, then `-- <path>` when there is a path.
fn command_args<'a>(head: &[&'a str], base: &'a str, pathspec: &[&'a str]) -> Vec<&'a str> {
    head.iter().copied().chain([base]).chain(pathspec.iter().copied()).collect()
}

async fn collect_changes(cwd: &Path, args: &DiffArgs) -> Result<GitChanges, Collected> {
    if !cwd.is_dir() {
        return Err(Collected::Failed(format!("Working directory not found: {}", cwd.display())));
    }
    let inside = git(cwd, &["rev-parse", "--is-inside-work-tree"]).await?;
    if !inside.status.success() || String::from_utf8_lossy(&inside.stdout).trim() != "true" {
        let error = first_line(&inside.stderr);
        let not_repository = inside.status.success() || error.contains("not a git repository");
        let failed = || Collected::Failed(format!("git rev-parse failed: {error}"));
        return Err(if not_repository { Collected::NotRepository } else { failed() });
    }
    // Before the first commit, compare with the empty tree.
    let head = git(cwd, &["rev-parse", "--verify", "--quiet", "HEAD"]).await?;
    let base = if head.status.success() {
        "HEAD".to_string()
    } else {
        git_stdout(cwd, &["hash-object", "-t", "tree", "--stdin"]).await?.trim().to_string()
    };
    let pathspec: Vec<&str> = args.path.as_deref().map(|path| vec!["--", path]).unwrap_or_default();
    let stat_args = command_args(&["diff", "--no-color", "--no-ext-diff", "--stat"], &base, &pathspec);
    let stat = git_stdout(cwd, &stat_args).await?;
    let diff = if args.stat_only {
        String::new()
    } else {
        git_stdout(cwd, &command_args(&["diff", "--no-color", "--no-ext-diff"], &base, &pathspec)).await?
    };
    let untracked_args = ["ls-files", "--others", "--exclude-standard"].into_iter().chain(pathspec.iter().copied());
    let untracked = git_stdout(cwd, &untracked_args.collect::<Vec<_>>()).await?;
    Ok(GitChanges { stat, untracked, diff })
}

async fn collect(cwd: PathBuf, args: DiffArgs) -> Collected {
    match collect_changes(&cwd, &args).await {
        Ok(changes) if changes.is_empty() => Collected::Clean,
        Ok(changes) => Collected::Changes(truncate_lines(report_lines(&changes), MAX_DIFF_LINES)),
        Err(outcome) => outcome,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineKind {
    Plain,
    Heading,
    FileHeader,
    Hunk,
    Added,
    Removed,
    Context,
    Note,
}

/// Lines without control characters; the text component expands tabs.
fn clean_lines(text: &str) -> Vec<String> {
    sanitize_binary_output(text).replace('\r', "").lines().map(str::to_string).collect()
}

fn report_lines(changes: &GitChanges) -> Vec<(LineKind, String)> {
    let mut lines: Vec<(LineKind, String)> = Vec::new();
    let separate = |lines: &mut Vec<(LineKind, String)>| {
        if !lines.is_empty() {
            lines.push((LineKind::Plain, String::new()));
        }
    };
    lines.extend(clean_lines(&changes.stat).into_iter().map(|line| (LineKind::Plain, line)));
    let untracked = clean_lines(&changes.untracked);
    if !untracked.is_empty() {
        separate(&mut lines);
        lines.push((LineKind::Heading, format!("Untracked files ({}):", untracked.len())));
        lines.extend(untracked.into_iter().map(|path| (LineKind::Plain, format!("  {path}"))));
    }
    let diff = clean_lines(&changes.diff);
    if !diff.is_empty() {
        separate(&mut lines);
    }
    // File headers run from `diff --git` to the first hunk, so `--- a/x` is not a removed line.
    let mut in_header = false;
    for line in diff {
        let kind = if line.starts_with("diff ") {
            in_header = true;
            LineKind::FileHeader
        } else if line.starts_with("@@") {
            in_header = false;
            LineKind::Hunk
        } else if in_header {
            LineKind::FileHeader
        } else {
            match line.chars().next() {
                Some('+') => LineKind::Added,
                Some('-') => LineKind::Removed,
                Some(' ') => LineKind::Context,
                _ => LineKind::Note,
            }
        };
        lines.push((kind, line));
    }
    lines
}

/// Keeps the first `max` lines and appends a note with the number of lines cut.
fn truncate_lines(mut lines: Vec<(LineKind, String)>, max: usize) -> Vec<(LineKind, String)> {
    if lines.len() > max {
        let cut = lines.len() - max;
        lines.truncate(max);
        let noun = if cut == 1 { "line" } else { "lines" };
        lines.push((LineKind::Plain, String::new()));
        lines.push((LineKind::Note, format!("... {cut} more {noun} not shown (diff exceeds {max} lines)")));
    }
    lines
}

fn paint(kind: LineKind, line: &str) -> String {
    let theme = theme();
    match kind {
        LineKind::Plain => line.to_string(),
        LineKind::Heading | LineKind::FileHeader => theme.bold(line),
        LineKind::Hunk => theme.fg(ThemeColor::Accent, line),
        LineKind::Added => theme.fg(ThemeColor::ToolDiffAdded, line),
        LineKind::Removed => theme.fg(ThemeColor::ToolDiffRemoved, line),
        LineKind::Context => theme.fg(ThemeColor::ToolDiffContext, line),
        LineKind::Note => theme.fg(ThemeColor::Dim, line),
    }
}

impl InteractiveMode {
    /// `/diff [--stat] [path]`: git runs on a worker task, and the result lands in the chat.
    pub(super) fn handle_diff_command(&self, args: Option<&str>) {
        let args = parse_diff_args(args);
        let cwd = self.runtime.cwd();
        self.spawn(move |mode| async move {
            let task = tokio::spawn(collect(cwd.clone(), args.clone()));
            let collected = task.await.unwrap_or_else(|error| Collected::Failed(error.to_string()));
            let scope = args.path.as_deref().map(|path| format!(" in {path}")).unwrap_or_default();
            match collected {
                Collected::GitMissing => mode.show_error("git not found on PATH"),
                Collected::NotRepository => mode.show_status(&format!("Not a git repository: {}", cwd.display())),
                Collected::Failed(error) => mode.show_error(&error),
                Collected::Clean => mode.show_status(&format!("No uncommitted changes{scope}")),
                Collected::Changes(lines) => {
                    let suffix = if args.stat_only { " (stat)" } else { "" };
                    mode.add_diff_box(format!("Uncommitted changes{scope}{suffix}"), lines);
                }
            }
        });
    }

    fn add_diff_box(&self, title: String, lines: Vec<(LineKind, String)>) {
        self.add_spacer();
        self.add_to_chat(component(DynamicBorder::new()));
        self.add_text(move || theme().bold(&theme().fg(ThemeColor::Accent, &title)), 1, 0);
        self.add_spacer();
        let render = move || lines.iter().map(|(kind, line)| paint(*kind, line)).collect::<Vec<_>>().join("\n");
        self.add_text(render, 1, 0);
        self.add_spacer();
        self.add_to_chat(component(DynamicBorder::new()));
        self.request_render();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(stat_only: bool, path: Option<&str>) -> DiffArgs {
        DiffArgs { stat_only, path: path.map(str::to_string) }
    }

    #[test]
    fn parses_stat_flag_and_path() {
        assert_eq!(parse_diff_args(None), args(false, None));
        assert_eq!(parse_diff_args(Some("--stat")), args(true, None));
        assert_eq!(parse_diff_args(Some("--stat src/lib.rs")), args(true, Some("src/lib.rs")));
        assert_eq!(parse_diff_args(Some("src/lib.rs --stat")), args(true, Some("src/lib.rs")));
        assert_eq!(parse_diff_args(Some("\"my dir/a.txt\"")), args(false, Some("my dir/a.txt")));
        assert_eq!(parse_diff_args(Some("--statistics")), args(false, Some("--statistics")));
    }

    #[test]
    fn classifies_headers_hunks_and_changed_lines() {
        let diff = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n--- sql\n+new\n same\n\\ No newline at end of file\n";
        let changes = GitChanges { stat: " x | 2 +-\n".into(), untracked: "y.txt\n".into(), diff: diff.into() };
        let kinds: Vec<LineKind> = report_lines(&changes).into_iter().map(|(kind, _)| kind).collect();
        use LineKind::*;
        let expected = [Plain, Plain, Heading, Plain, Plain, FileHeader, FileHeader, FileHeader, FileHeader, Hunk];
        assert_eq!(kinds[..10], expected);
        assert_eq!(kinds[10..], [Removed, Added, Context, Note]);
    }

    #[test]
    fn strips_control_characters() {
        let changes = GitChanges { diff: "+a\x1b[31mred\r\n".into(), ..GitChanges::default() };
        assert_eq!(report_lines(&changes), [(LineKind::Added, "+a[31mred".to_string())]);
    }

    #[test]
    fn truncates_with_a_note() {
        let lines: Vec<(LineKind, String)> = (0..12).map(|n| (LineKind::Context, format!(" {n}"))).collect();
        let kept = truncate_lines(lines.clone(), 10);
        assert_eq!(kept.len(), 12);
        assert_eq!(kept[11].1, "... 2 more lines not shown (diff exceeds 10 lines)");
        assert_eq!(truncate_lines(lines, 12).len(), 12);
    }
}
