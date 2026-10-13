//! Moving between sessions and tree points: `/tree` with the branch summary prompt, and
//! `/resume` (upstream `showTreeSelector`, `showSessionSelector`, `handleResumeSession`).

use std::path::PathBuf;
use std::rc::Rc;

use pi_tui::component;

use super::components::{StatusIndicator, StatusKind};
use super::mode::InteractiveMode;
use super::selector::SelectorSpec;
use crate::agent_session::NavigateTreeOptions;
use crate::interactive_components::session_selector::{
    SessionSelectorOptions, all_sessions_loader, project_sessions_loader,
};
use crate::interactive_components::tree_selector::{
    ALREADY_AT_TARGET_STATUS, BRANCH_SUMMARY_PROMPT_TITLE, BranchSummaryChoice, BranchSummaryFlow, BranchSummaryStep,
    CUSTOM_INSTRUCTIONS_TITLE,
};
use crate::interactive_components::{SessionSelector, TreeData, TreeSelector};
use crate::session::SessionManager;
use crate::session::branching::missing_session_cwd;

impl InteractiveMode {
    /// `/tree` and its key: move to another point of the session tree.
    pub(super) fn show_tree_selector(&self, initial_selected: Option<String>) {
        let session = self.session();
        let (tree, leaf): (TreeData, Option<String>) = {
            let manager = session.session_manager();
            (TreeData::from(manager.get_tree()), manager.leaf_id().map(String::from))
        };
        if tree.is_empty() {
            return self.show_status("No entries in session");
        }
        let filter = session.settings().tree_filter_mode();
        let rows = self.terminal.borrow().rows();
        let weak = self.weak.clone();
        self.selector.show(Box::new(move |done| {
            let (select, cancel, label, copy) = (weak.clone(), weak.clone(), weak.clone(), weak);
            let (done_select, done_cancel) = (done.clone(), done);
            let mut selector = TreeSelector::new(
                tree,
                leaf.as_deref(),
                rows,
                move |entry_id| {
                    done_select();
                    if let Some(mode) = select.upgrade() {
                        mode.start_tree_navigation(entry_id.to_string());
                    }
                },
                move || {
                    done_cancel();
                    if let Some(mode) = cancel.upgrade() {
                        mode.request_render();
                    }
                },
                Some(Box::new(move |entry_id: &str, text: Option<&str>| {
                    if let Some(mode) = label.upgrade() {
                        let _ = mode.session().session_manager().append_label_change(entry_id, text);
                        mode.request_render();
                    }
                })),
                initial_selected.as_deref(),
                Some(filter),
            );
            selector.set_on_copy(move |text| {
                let Some(mode) = copy.upgrade() else { return };
                match text {
                    None => mode.show_error("Selected entry has no text to copy"),
                    Some(text) => match crate::clipboard::copy_to_clipboard(text) {
                        Ok(()) => mode.show_status("Copied selected message to clipboard"),
                        Err(error) => mode.show_error(&error.to_string()),
                    },
                }
            });
            SelectorSpec::new(component(selector))
        }));
    }

    /// The summary prompt after a tree selection, then the navigation.
    fn start_tree_navigation(&self, entry_id: String) {
        let session = self.session();
        let leaf = session.session_manager().leaf_id().map(String::from);
        let skip_prompt = session.settings().branch_summary_settings().1;
        let (mut flow, mut step) = BranchSummaryFlow::start(&entry_id, leaf.as_deref(), skip_prompt);
        self.spawn(move |mode| async move {
            loop {
                step = match step {
                    BranchSummaryStep::AlreadyAtTarget => return mode.show_status(ALREADY_AT_TARGET_STATUS),
                    BranchSummaryStep::ChooseSummary => {
                        let options = BranchSummaryChoice::options();
                        let choice = mode.ask_choice(BRANCH_SUMMARY_PROMPT_TITLE, options).await.ok().flatten();
                        flow.choose(choice.as_deref())
                    }
                    BranchSummaryStep::EditInstructions => {
                        let text = mode.ask_editor(CUSTOM_INSTRUCTIONS_TITLE, None).await.ok().flatten();
                        flow.custom_instructions(text)
                    }
                    BranchSummaryStep::ReopenTree { entry_id } => return mode.show_tree_selector(Some(entry_id)),
                    BranchSummaryStep::Navigate { entry_id, options } => {
                        return mode.navigate_tree(entry_id, options).await;
                    }
                };
            }
        });
    }

    async fn navigate_tree(self: Rc<Self>, entry_id: String, options: NavigateTreeOptions) {
        let session = self.session();
        if session.is_streaming() {
            self.restore_queued_messages_to_editor(false, None);
            let aborting = session.clone();
            let _ = tokio::spawn(async move { aborting.abort().await }).await;
        }
        if session.is_compacting() {
            return self.show_error(
                "Wait for the current compaction or tree navigation to finish before navigating the session tree.",
            );
        }
        let summarize = options.summarize;
        let mut replaced = None;
        if summarize {
            let abort = self.action(|mode| mode.session().abort_branch_summary());
            replaced = Some(self.editor.set_on_escape(Some(abort)));
            self.add_spacer();
            self.show_status_indicator(StatusIndicator::branch_summary(&self.tui()));
        }
        let target = entry_id.clone();
        let result = tokio::spawn(async move { session.navigate_tree(&target, options).await }).await;
        if summarize {
            self.clear_status_indicator(Some(StatusKind::BranchSummary));
        }
        if let Some(handler) = replaced {
            self.editor.set_on_escape(handler);
        }
        match result {
            Ok(Ok(result)) if result.aborted => {
                self.show_status("Branch summarization cancelled");
                self.show_tree_selector(Some(entry_id));
            }
            Ok(Ok(result)) if result.cancelled => self.show_status("Navigation cancelled"),
            Ok(Ok(result)) => {
                self.ui.chat.borrow_mut().clear();
                self.render_initial_messages();
                if let Some(text) = result.editor_text.filter(|_| self.editor.get_text().trim().is_empty()) {
                    self.editor.set_text(&text);
                }
                self.show_status("Navigated to selected point");
                self.flush_compaction_queue(false);
            }
            Ok(Err(error)) => self.show_error(&error.to_string()),
            Err(error) => self.show_error(&error.to_string()),
        }
    }

    /// `/resume` and its key: open another session.
    pub(super) fn show_session_selector(&self) {
        let session = self.session();
        let (cwd, dir, current, default_dir) = {
            let manager = session.session_manager();
            (
                manager.cwd().to_path_buf(),
                manager.session_dir().to_path_buf(),
                manager.session_file().map(|file| file.to_path_buf()),
                manager.uses_default_session_dir(&self.agent_dir),
            )
        };
        let agent_dir = self.agent_dir.clone();
        let (weak, render, exit) = (self.weak.clone(), self.weak.clone(), self.weak.clone());
        self.selector.show(Box::new(move |done| {
            let (select, cancel) = (weak.clone(), weak);
            let (done_select, done_cancel) = (done.clone(), done);
            let custom = (!default_dir).then(|| dir.clone());
            let selector = SessionSelector::new(
                project_sessions_loader(cwd, Some(dir), agent_dir.clone()),
                all_sessions_loader(custom, agent_dir),
                move |path| {
                    done_select();
                    if let Some(mode) = select.upgrade() {
                        mode.handle_resume_session(path);
                    }
                },
                move || {
                    done_cancel();
                    if let Some(mode) = cancel.upgrade() {
                        mode.request_render();
                    }
                },
                move || {
                    if let Some(mode) = exit.upgrade() {
                        mode.request_shutdown();
                    }
                },
                move || {
                    if let Some(mode) = render.upgrade() {
                        mode.request_render();
                    }
                },
                SessionSelectorOptions {
                    rename_session: Some(Box::new(|path, name| {
                        let mut manager = SessionManager::open_with(&path, None, None)?;
                        manager.append_session_info(&name)?;
                        Ok(())
                    })),
                    show_rename_hint: Some(true),
                },
                current.as_deref(),
            );
            SelectorSpec::new(selector)
        }));
    }

    /// Switch to `path`, asking to continue here when its working directory is gone.
    fn handle_resume_session(&self, path: PathBuf) {
        self.clear_status_indicator(None);
        let runtime = self.runtime.clone();
        self.spawn(move |mode| async move {
            let fallback = runtime.cwd();
            let missing = SessionManager::open_with(&path, None, None)
                .ok()
                .and_then(|manager| missing_session_cwd(&manager, &fallback));
            let cwd_override = match missing {
                None => None,
                Some(missing) => {
                    if !mode.ask_confirm("Session cwd not found", &missing.prompt()).await {
                        return mode.show_status("Resume cancelled");
                    }
                    Some(missing.fallback_cwd)
                }
            };
            let resumed_here = cwd_override.is_some();
            let switch = async move { runtime.switch_session(&path, cwd_override.as_deref()).await };
            match tokio::spawn(switch).await {
                Ok(Ok(result)) if result.cancelled => {}
                Ok(Ok(_)) if resumed_here => mode.show_status("Resumed session in current cwd"),
                Ok(Ok(_)) => mode.show_status("Resumed session"),
                Ok(Err(error)) => mode.show_error(&format!("Failed to resume session: {error:#}")),
                Err(error) => mode.show_error(&format!("Failed to resume session: {error}")),
            }
        });
    }
}
