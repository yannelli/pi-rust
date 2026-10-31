//! `/undo` (pi-rust extension): restore the files the edit and write tools changed during the
//! newest prompt that changed any. The conversation stays as it is.

use std::path::Path;

use super::mode::InteractiveMode;
use super::util::format_path_relative_to_cwd_or_absolute;
use crate::agent_session::{UndoReport, UndoSkip};
use crate::extension_ui::{DialogOptions, ExtensionUi};
use crate::theme::{ThemeColor, theme};

const NOTE: &str = "Bash tool changes are not tracked, and the conversation is unchanged (use /tree for that).";
const RESTORE_ALL: &str = "Restore all of them";
const RESTORE_REPORTED: &str = "Restore only the changes the agent reported";

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
            UndoSkip::Unreported => "the agent did not report this change".to_string(),
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
        let candidates = session.undo_candidates();
        if candidates.is_empty() {
            return self.show_status("Nothing to undo: no edit or write tool changes recorded in this session.");
        }
        let unreported: Vec<String> = candidates
            .iter()
            .filter(|candidate| candidate.detected)
            .map(|candidate| format_path_relative_to_cwd_or_absolute(&candidate.path, session.cwd()))
            .collect();
        if unreported.is_empty() {
            return self.show_undo_report(session.undo_file_changes());
        }
        self.spawn(move |mode| async move {
            let title = format!(
                "These files changed during the ACP agent's turn, but the agent did not report the change. \
                 You or another program may have changed them:\n{}",
                unreported.join("\n")
            );
            let options = [RESTORE_ALL.to_string(), RESTORE_REPORTED.to_string()];
            let Some(choice) = mode.extension_ui().select(&title, &options, DialogOptions::default()).await else {
                return mode.show_status("Undo cancelled");
            };
            let session = mode.session();
            mode.show_undo_report(session.undo_file_changes_with(choice == RESTORE_ALL));
        });
    }

    fn show_undo_report(&self, report: Option<UndoReport>) {
        let Some(report) = report else { return };
        let lines = undo_summary(&report, self.session().cwd());
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
