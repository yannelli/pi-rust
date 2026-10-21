//! `/undo` (pi-rust extension): restore the files the edit and write tools changed during the
//! newest prompt that changed any. The conversation stays as it is.

use std::path::Path;

use super::mode::InteractiveMode;
use super::util::format_path_relative_to_cwd_or_absolute;
use crate::agent_session::{UndoReport, UndoSkip};
use crate::theme::{ThemeColor, theme};

const NOTE: &str = "Bash tool changes are not tracked, and the conversation is unchanged (use /tree for that).";

/// One line of the `/undo` summary and its color.
type Line = (ThemeColor, String);

/// The lines `/undo` prints for `report`, with paths relative to `cwd` when inside it.
pub(super) fn undo_summary(report: &UndoReport, cwd: &Path) -> Vec<Line> {
    let path = |file: &Path| format_path_relative_to_cwd_or_absolute(file, cwd);
    let mut lines = vec![(ThemeColor::Accent, "Undid the file changes of the last prompt".to_string())];
    lines.extend(report.restored.iter().map(|file| (ThemeColor::Success, format!("  restored  {}", path(file)))));
    lines.extend(report.deleted.iter().map(|file| (ThemeColor::Success, format!("  deleted   {}", path(file)))));
    for (file, reason) in &report.skipped {
        let reason = match reason {
            UndoSkip::ChangedSince => "changed since the agent wrote it".to_string(),
            UndoSkip::NotRestorable(reason) => reason.clone(),
            UndoSkip::Failed(error) => format!("could not restore it: {error}"),
        };
        lines.push((ThemeColor::Warning, format!("  skipped   {} ({reason})", path(file))));
    }
    if report.remaining > 0 {
        let count = report.remaining;
        let prompts = if count == 1 { "prompt has" } else { "prompts have" };
        lines.push((ThemeColor::Dim, format!("{count} earlier {prompts} changes that /undo can restore.")));
    }
    lines.push((ThemeColor::Dim, NOTE.to_string()));
    lines
}

impl InteractiveMode {
    pub(super) fn handle_undo_command(&self) {
        let session = self.session();
        if session.is_streaming() {
            return self
                .show_warning("Wait for the current response to finish, or press Esc to abort it, before /undo.");
        }
        let Some(report) = session.undo_file_changes() else {
            return self.show_status("Nothing to undo: no edit or write tool changes recorded in this session.");
        };
        let lines = undo_summary(&report, session.cwd());
        self.add_spacer();
        self.add_text(
            move || lines.iter().map(|(color, text)| theme().fg(*color, text)).collect::<Vec<_>>().join("\n"),
            1,
            0,
        );
        self.request_render();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_paths_relative_to_the_cwd() {
        let cwd = std::env::temp_dir().join("undo-work");
        let report = UndoReport {
            restored: vec![cwd.join("src").join("main.rs")],
            deleted: vec![cwd.join("new.txt")],
            skipped: vec![(std::env::temp_dir().join("elsewhere").join("x.txt"), UndoSkip::ChangedSince)],
            remaining: 2,
        };
        let text: Vec<String> = undo_summary(&report, &cwd).into_iter().map(|(_, line)| line).collect();
        assert_eq!(text[1], "  restored  src/main.rs");
        assert_eq!(text[2], "  deleted   new.txt");
        assert!(text[3].starts_with("  skipped   ") && text[3].ends_with("x.txt (changed since the agent wrote it)"));
        assert_eq!(text[4], "2 earlier prompts have changes that /undo can restore.");
        assert_eq!(text[5], NOTE);
    }
}
