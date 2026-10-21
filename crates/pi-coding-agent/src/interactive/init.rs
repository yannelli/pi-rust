//! `/init` (pi-rust extension): ask the agent to write or improve the project's context file.

use std::path::Path;

use super::mode::InteractiveMode;
use crate::resources::context::CONTEXT_FILE_NAMES;

const NEW_FILE: &str = "Inspect the project in the current working directory and write an AGENTS.md file there. \
Coding agents load this file as project context.";

const COVER: &str = "Cover:
- Project overview: what the project does and its main parts.
- Build, test, and lint commands, including how to run a single test.
- Code layout: the main directories and what they hold.
- Conventions: code style, naming, and patterns the code follows.
- Gotchas: setup steps, pitfalls, and behavior that is not obvious.

Base every statement on what you find in the repository, such as the README, build files, CI \
configuration, and source code. Do not guess and do not add generic advice. Keep the file concise \
and leave out sections that have nothing useful.";

/// The context file pi loads from `cwd`, if one exists.
fn existing_context_file(cwd: &Path) -> Option<&'static str> {
    CONTEXT_FILE_NAMES.into_iter().find(|name| cwd.join(name).is_file())
}

/// The `/init` prompt for `cwd`, with `extra` instructions at the end.
pub(super) fn init_prompt(cwd: &Path, extra: Option<&str>) -> String {
    let intro = match existing_context_file(cwd) {
        Some(name) => format!(
            "Inspect the project in the current working directory and improve its existing {name} file in \
             place. Keep what is accurate, correct what is out of date, and add what is missing. Do not replace \
             the file with a new one."
        ),
        None => NEW_FILE.to_string(),
    };
    let mut prompt = format!("{intro}\n\n{COVER}");
    if let Some(extra) = extra {
        prompt.push_str("\n\nAdditional instructions:\n");
        prompt.push_str(extra);
    }
    prompt
}

impl InteractiveMode {
    /// `/init [instructions]`: send the prompt as if typed, keeping `/init` in the editor history.
    pub(super) fn handle_init_command(&self, typed: &str, extra: Option<&str>) {
        let prompt = init_prompt(self.session().cwd(), extra);
        self.send_prompt(prompt, typed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_for_a_new_file_or_improves_the_existing_one() {
        let dir = tempfile::tempdir().unwrap();
        let prompt = init_prompt(dir.path(), None);
        assert!(prompt.starts_with(NEW_FILE), "{prompt}");
        assert!(!prompt.contains("Additional instructions"), "{prompt}");

        std::fs::write(dir.path().join("CLAUDE.md"), "# Notes\n").unwrap();
        let prompt = init_prompt(dir.path(), Some("Mention the release steps."));
        assert!(prompt.contains("improve its existing CLAUDE.md file in place"), "{prompt}");
        assert!(prompt.ends_with("Additional instructions:\nMention the release steps."), "{prompt}");
    }
}
