//! `/review` (pi-rust extension): ask the agent to review the uncommitted git changes in the
//! session cwd and report problems without changing files.

use super::git_diff::work_tree_problem;
use super::mode::InteractiveMode;

const REVIEW: &str = "Review the uncommitted changes in the current working directory. Do not edit, create, or \
delete files, and do not run commands that change the repository, unless I ask you to.

Steps:
1. Run `git status` to see the staged, unstaged, and untracked files.
2. Run `git diff HEAD` to see the staged and unstaged changes. If the repository has no commits yet, run \
`git diff --cached` and `git diff` instead.
3. Read the untracked files that belong to the change, because `git diff HEAD` does not show them.
4. Read the code around a change when you need more context to judge it.

Report the concrete problems you find, ordered by severity with the most severe first:
- Bugs: logic errors, crashes, data loss, and behavior that the change breaks.
- Risky changes: security issues, concurrency problems, changed public APIs or file formats, and missing \
error handling.
- Missing tests: changed behavior that no test covers.

For each problem, give its location as `path:line`, explain what goes wrong and when, and suggest a fix. \
Leave out style comments unless they hide a bug. If you find no problems, say so.";

const EXTRA: &str = "Additional instructions (if they name a base branch or commit, run `git diff <ref>` in \
place of `git diff HEAD`):";

/// The `/review` prompt with `extra` instructions at the end.
pub(super) fn review_prompt(extra: Option<&str>) -> String {
    match extra {
        Some(extra) => format!("{REVIEW}\n\n{EXTRA}\n{extra}"),
        None => REVIEW.to_string(),
    }
}

impl InteractiveMode {
    /// `/review [instructions]`: outside a git work tree, an error and no prompt. Otherwise the
    /// prompt goes out as if typed, with `/review` in the editor history.
    pub(super) fn handle_review_command(&self, typed: &str, extra: Option<&str>) {
        let prompt = review_prompt(extra);
        let typed = typed.to_string();
        let cwd = self.runtime.cwd();
        self.spawn(move |mode| async move {
            let check = tokio::spawn(work_tree_problem(cwd)).await;
            match check.unwrap_or_else(|error| Some(error.to_string())) {
                Some(problem) => mode.show_error(&problem),
                None => mode.send_prompt(prompt, &typed),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_for_git_commands_and_appends_extra_instructions() {
        let prompt = review_prompt(None);
        assert!(prompt.contains("`git status`") && prompt.contains("`git diff HEAD`"), "{prompt}");
        assert!(prompt.contains("`path:line`") && prompt.contains("Do not edit"), "{prompt}");
        assert!(!prompt.contains("Additional instructions"), "{prompt}");

        let prompt = review_prompt(Some("Focus on error handling."));
        assert!(prompt.starts_with(REVIEW), "{prompt}");
        assert!(prompt.ends_with(&format!("{EXTRA}\nFocus on error handling.")), "{prompt}");
    }
}
