//! Tree navigation with branch summaries, labels, names, statistics, context usage, export,
//! and resource reload (upstream `navigateTree`, `getSessionStats`, `exportToHtml`, `reload`).

use std::path::{Path, PathBuf};

use pi_ai::{Message, Usage};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{AgentSession, SummarizationSource, lock};
use crate::compaction::utils::user_content_text;
use crate::compaction::{
    BranchSummaryOptions, CompactionDetails, ContextUsage, collect_entries_for_branch_summary, context_usage,
    generate_branch_summary,
};
use crate::export::{ExportTheme, ExportTool, HtmlExportOptions, export_session_html};
use crate::session::usage::SessionStats;
use crate::session::{Entry, EntryData};

/// Options of [`AgentSession::navigate_tree`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NavigateTreeOptions {
    /// Summarize the abandoned branch.
    pub summarize: bool,
    pub custom_instructions: Option<String>,
    /// Use `custom_instructions` in place of the default summary prompt.
    pub replace_instructions: bool,
    /// Label for the summary entry, or the target when there is no summary.
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NavigateTreeResult {
    /// Text for the editor when the target was a user or custom message.
    pub editor_text: Option<String>,
    pub cancelled: bool,
    pub aborted: bool,
    pub summary_entry_id: Option<String>,
}

/// `/session` statistics plus current context use (upstream `SessionStats`).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionStatsReport {
    pub stats: SessionStats,
    pub context_usage: Option<ContextUsage>,
}

impl SessionStatsReport {
    /// The RPC `get_session_stats` shape.
    pub fn to_json(&self) -> Value {
        let stats = &self.stats;
        let mut value = json!({
            "sessionFile": stats.session_file.as_ref().map(|path| path.display().to_string()),
            "sessionId": stats.session_id,
            "userMessages": stats.user_messages,
            "assistantMessages": stats.assistant_messages,
            "toolCalls": stats.tool_calls,
            "toolResults": stats.tool_results,
            "totalMessages": stats.total_messages,
            "tokens": {
                "input": stats.tokens.input,
                "output": stats.tokens.output,
                "cacheRead": stats.tokens.cache_read,
                "cacheWrite": stats.tokens.cache_write,
                "total": stats.tokens.tokens(),
            },
            "cost": stats.cost,
        });
        if stats.session_file.is_none() {
            value.as_object_mut().map(|object| object.remove("sessionFile"));
        }
        if let Some(usage) = &self.context_usage {
            value["contextUsage"] =
                json!({"tokens": usage.tokens, "contextWindow": usage.context_window, "percent": usage.percent});
        }
        value
    }
}

type PendingSummary = (String, Option<Value>, Option<Usage>);

/// What `session_before_tree` handlers decided.
enum BeforeTree {
    Continue,
    Cancelled,
    Summary(Box<PendingSummary>),
}

impl AgentSession {
    /// Move to another entry of the session tree in the same file, summarizing the abandoned
    /// branch when asked. A user or custom message target moves to its parent.
    pub async fn navigate_tree(
        &self,
        target_id: &str,
        options: NavigateTreeOptions,
    ) -> anyhow::Result<NavigateTreeResult> {
        if self.is_streaming() {
            anyhow::bail!("Wait for the current response to finish before navigating the session tree.");
        }
        if self.is_compacting() {
            anyhow::bail!(
                "Wait for the current compaction or tree navigation to finish before navigating the session tree."
            );
        }
        let (old_leaf, entries, common_ancestor) = {
            let session = self.session_manager();
            let old_leaf = session.leaf_id().map(String::from);
            if old_leaf.as_deref() == Some(target_id) {
                return Ok(NavigateTreeResult::default());
            }
            if session.get_entry(target_id).is_none() {
                anyhow::bail!("Entry {target_id} not found");
            }
            let (entries, ancestor) = collect_entries_for_branch_summary(&session, old_leaf.as_deref(), target_id);
            (old_leaf, entries.into_iter().cloned().collect::<Vec<Entry>>(), ancestor)
        };
        let model = self.model();
        if options.summarize && model.is_none() {
            anyhow::bail!("No model available for summarization");
        }
        let cancel = CancellationToken::new();
        lock(&self.state).branch_summary = Some(cancel.clone());
        self.notify_idle();
        let mut options = options;
        let hooked = self
            .before_tree(target_id, old_leaf.as_deref(), common_ancestor.as_deref(), &entries, &mut options, &cancel)
            .await;
        let from_extension = matches!(hooked, BeforeTree::Summary(_));
        let summary = match hooked {
            BeforeTree::Cancelled => Ok(Some(Err(NavigateTreeResult { cancelled: true, ..Default::default() }))),
            BeforeTree::Summary(summary) => Ok(Some(Ok(*summary))),
            BeforeTree::Continue => match (options.summarize && !entries.is_empty(), model) {
                (true, Some(model)) => self.branch_summary(&entries, &model, &options, cancel).await,
                _ => Ok(None),
            },
        };
        lock(&self.state).branch_summary = None;
        self.notify_idle();
        let summary = match summary {
            Ok(Some(Ok(summary))) => Some(summary),
            Ok(Some(Err(aborted))) => return Ok(aborted),
            Ok(None) => None,
            Err(error) => return Err(error),
        };
        let summarized = summary.is_some();
        let result = self.apply_navigation(target_id, summary, options.label.as_deref(), from_extension)?;
        self.refresh_context();
        self.restore_tools_from_transcript(&[]);
        if let Some(runner) = self.extension_runner() {
            let (new_leaf, summary_entry) = {
                let session = self.session_manager();
                let entry = result.summary_entry_id.as_deref().and_then(|id| session.get_entry(id));
                (session.leaf_id().map(String::from), entry.map(|entry| Value::Object(entry.raw().clone())))
            };
            let mut event = json!({"type": "session_tree", "newLeafId": new_leaf, "oldLeafId": old_leaf});
            if let Some(entry) = summary_entry {
                event["summaryEntry"] = entry;
            }
            if summarized {
                event["fromExtension"] = json!(from_extension);
            }
            runner.emit(event).await;
        }
        Ok(result)
    }

    /// `session_before_tree`: cancel, a summary, or changed instructions and label.
    async fn before_tree(
        &self,
        target_id: &str,
        old_leaf: Option<&str>,
        common_ancestor: Option<&str>,
        entries: &[Entry],
        options: &mut NavigateTreeOptions,
        cancel: &CancellationToken,
    ) -> BeforeTree {
        let Some(runner) = self.extension_runner().filter(|runner| runner.has_handlers("session_before_tree")) else {
            return BeforeTree::Continue;
        };
        let signal = runner.signal_id(cancel);
        let preparation = json!({
            "targetId": target_id,
            "oldLeafId": old_leaf,
            "commonAncestorId": common_ancestor,
            "entriesToSummarize": entries.iter().map(|entry| Value::Object(entry.raw().clone())).collect::<Vec<_>>(),
            "userWantsSummary": options.summarize,
            "customInstructions": options.custom_instructions,
            "replaceInstructions": options.replace_instructions,
            "label": options.label,
        });
        let event = json!({"type": "session_before_tree", "preparation": preparation, "signal": signal});
        let result = runner.emit(event).await;
        runner.release_signal_id(signal);
        if result["cancel"].as_bool() == Some(true) {
            return BeforeTree::Cancelled;
        }
        if let Some(text) = result["customInstructions"].as_str() {
            options.custom_instructions = Some(text.to_string());
        }
        if let Some(replace) = result["replaceInstructions"].as_bool() {
            options.replace_instructions = replace;
        }
        if let Some(label) = result["label"].as_str() {
            options.label = Some(label.to_string());
        }
        match result["summary"]["summary"].as_str() {
            Some(text) if options.summarize => {
                let details = result["summary"].get("details").filter(|details| !details.is_null()).cloned();
                let usage = serde_json::from_value(result["summary"]["usage"].clone()).ok();
                BeforeTree::Summary(Box::new((text.to_string(), details, usage)))
            }
            _ => BeforeTree::Continue,
        }
    }

    async fn branch_summary(
        &self,
        entries: &[Entry],
        model: &pi_ai::Model,
        options: &NavigateTreeOptions,
        cancel: CancellationToken,
    ) -> anyhow::Result<Option<Result<PendingSummary, NavigateTreeResult>>> {
        let (model, summarization) =
            self.summarization_options(model, cancel, SummarizationSource::BranchSummary).await?;
        let model = &model;
        let summary_options = BranchSummaryOptions {
            summarization,
            custom_instructions: options.custom_instructions.clone(),
            replace_instructions: options.replace_instructions,
            reserve_tokens: self.settings().branch_summary_settings().0,
        };
        let refs: Vec<&Entry> = entries.iter().collect();
        let result = generate_branch_summary(&refs, model, &summary_options).await;
        if result.aborted {
            return Ok(Some(Err(NavigateTreeResult { cancelled: true, aborted: true, ..Default::default() })));
        }
        if let Some(error) = result.error {
            anyhow::bail!(error);
        }
        let details = CompactionDetails { read_files: result.read_files, modified_files: result.modified_files };
        Ok(result.summary.map(|text| Ok((text, serde_json::to_value(details).ok(), result.usage))))
    }

    fn apply_navigation(
        &self,
        target_id: &str,
        summary: Option<PendingSummary>,
        label: Option<&str>,
        from_extension: bool,
    ) -> anyhow::Result<NavigateTreeResult> {
        let mut session = self.session_manager();
        let target =
            session.get_entry(target_id).cloned().ok_or_else(|| anyhow::anyhow!("Entry {target_id} not found"))?;
        let (new_leaf, editor_text) = match target.data() {
            EntryData::Message(Message::User(user)) => {
                (target.parent_id().map(String::from), Some(user_content_text(&user.content, "")))
            }
            EntryData::CustomMessage(custom) => {
                (target.parent_id().map(String::from), Some(user_content_text(&custom.content, "")))
            }
            _ => (Some(target_id.to_string()), None),
        };
        let label = label.filter(|label| !label.is_empty());
        let mut summary_entry_id = None;
        match summary {
            Some((text, details, usage)) => {
                let id =
                    session.branch_with_summary(new_leaf.as_deref(), &text, details, Some(from_extension), usage)?;
                if let Some(label) = label {
                    session.append_label_change(&id, Some(label))?;
                }
                summary_entry_id = Some(id);
            }
            None => {
                match &new_leaf {
                    Some(id) => session.branch(id)?,
                    None => session.reset_leaf(),
                }
                if let Some(label) = label {
                    session.append_label_change(target_id, Some(label))?;
                }
            }
        }
        Ok(NavigateTreeResult { editor_text, cancelled: false, aborted: false, summary_entry_id })
    }

    /// Cancel a running branch summary.
    pub fn abort_branch_summary(&self) {
        if let Some(token) = &lock(&self.state).branch_summary {
            token.cancel();
        }
    }

    /// User messages of the whole session for the fork selector: `(entry id, text)`.
    pub fn user_messages_for_forking(&self) -> Vec<(String, String)> {
        let session = self.session_manager();
        session
            .entries()
            .iter()
            .filter_map(|entry| match entry.data() {
                EntryData::Message(Message::User(user)) => {
                    let text = user_content_text(&user.content, "");
                    (!text.is_empty()).then(|| (entry.id().to_string(), text))
                }
                _ => None,
            })
            .collect()
    }

    /// Set the session display name (an empty name clears it).
    pub fn set_session_name(&self, name: &str) -> anyhow::Result<()> {
        let name = {
            let mut session = self.session_manager();
            session.append_session_info(name)?;
            session.session_name()
        };
        if let Some(runner) = self.extension_runner() {
            runner.emit_detached(serde_json::json!({"type": "session_info_changed", "name": name}));
        }
        self.emit(&super::AgentSessionEvent::SessionInfoChanged { name });
        Ok(())
    }

    /// Bookmark an entry; `None` or an empty label clears it.
    pub fn set_label(&self, entry_id: &str, label: Option<&str>) -> anyhow::Result<()> {
        self.session_manager().append_label_change(entry_id, label)?;
        Ok(())
    }

    /// Totals over every entry, including compacted history, plus the current context use.
    pub fn session_stats(&self) -> SessionStatsReport {
        let stats = self.session_manager().session_stats();
        SessionStatsReport { stats, context_usage: self.context_usage() }
    }

    /// Current context use of the active branch. `tokens` is unknown right after compaction.
    pub fn context_usage(&self) -> Option<ContextUsage> {
        let model = self.limits_model()?;
        context_usage(&self.session_manager(), model.context_window)
    }

    /// Export the session to HTML; returns the written path.
    pub fn export_to_html(&self, output_path: Option<&str>) -> anyhow::Result<PathBuf> {
        self.export_to_html_themed(output_path, None)
    }

    /// The active tools as the export lists them.
    pub fn export_tools(&self) -> Vec<ExportTool> {
        self.with_state(|state| {
            state
                .tools
                .iter()
                .map(|tool| ExportTool {
                    name: tool.name().to_string(),
                    description: Some(tool.description().to_string()),
                    parameters: tool.parameters(),
                })
                .collect()
        })
    }

    /// [`Self::export_to_html`] in `theme_name`, else the `theme` setting, when they exist.
    pub fn export_to_html_themed(
        &self,
        output_path: Option<&str>,
        theme_name: Option<&str>,
    ) -> anyhow::Result<PathBuf> {
        let Some(file) = self.session_file() else { anyhow::bail!("Cannot export in-memory session to HTML") };
        let name = [theme_name.map(str::to_string), self.settings().theme()]
            .into_iter()
            .flatten()
            .find(|name| crate::theme::get_theme_by_name(name).is_some());
        let theme = crate::theme::export_theme(name.as_deref()).unwrap_or_else(|_| ExportTheme::dark());
        let renderer = crate::export::AnsiToolRendererAdapter::new(super::export_tools::SessionToolRenderer::new(self));
        let options = HtmlExportOptions {
            theme,
            system_prompt: Some(self.system_prompt()),
            tools: Some(self.export_tools()),
            tool_renderer: Some(&renderer),
        };
        export_session_html(&file, output_path, &options)
    }

    /// Write the header and the current branch to a JSONL file; returns its path.
    pub fn export_to_jsonl(&self, output_path: Option<&Path>) -> anyhow::Result<PathBuf> {
        self.session_manager().export_branch_to_jsonl(output_path)
    }

    /// Reload settings and resources: queue modes, skills, prompt templates, context files, and
    /// system prompt files, keeping the resolved packages. `defaultTools` additions become active.
    pub fn reload(&self) {
        let previous_defaults = self.current_default_tools();
        self.settings().reload();
        self.reload_with_packages(None, &previous_defaults);
    }

    /// [`Self::reload`] that resolves the `packages` settings again, installing missing ones
    /// (`/reload`, upstream `ResourceLoader.reload`). Returns the package error, if any.
    pub async fn reload_resources(&self) -> Option<super::RuntimeDiagnostic> {
        let previous_defaults = self.current_default_tools();
        self.settings().reload();
        let (cwd, agent_dir, settings) = {
            let services = self.services();
            (services.cwd.clone(), services.agent_dir.clone(), services.settings.clone())
        };
        let (packages, error) = super::services::package_resources(&cwd, &agent_dir, &settings).await;
        self.reload_with_packages(Some(packages), &previous_defaults);
        error
    }

    fn reload_with_packages(&self, packages: Option<crate::resources::ResolvedPaths>, previous_defaults: &[String]) {
        self.sync_queue_modes();
        {
            let services = self.services();
            let mut loader = services.resources();
            let mut options = super::services::resource_loader_options(
                &services.cwd,
                &services.agent_dir,
                services.settings.as_ref(),
                &resource_options_of(&loader.options),
            );
            options.package_resources = packages.unwrap_or_else(|| loader.options.package_resources.clone());
            loader.options = options;
            loader.reload();
        }
        self.reload_tools(previous_defaults);
        self.rebuild_system_prompt();
    }
}

/// The CLI part of loader options, kept across reloads.
fn resource_options_of(options: &crate::resources::ResourceLoaderOptions) -> super::ResourceOptions {
    super::ResourceOptions {
        skill_paths: options.skill_paths.clone(),
        prompt_template_paths: options.prompt_template_paths.clone(),
        theme_paths: options.theme_paths.clone(),
        extension_sources: options.extension_sources.clone(),
        no_extensions: options.no_extensions,
        no_skills: options.no_skills,
        no_prompt_templates: options.no_prompt_templates,
        no_themes: options.no_themes,
        no_context_files: options.no_context_files,
        system_prompt: options.system_prompt.clone(),
        append_system_prompt: options.append_system_prompt.clone(),
        disabled_builtin_extensions: options.disabled_builtin_extensions.clone(),
        home: Some(options.home.clone()),
    }
}
