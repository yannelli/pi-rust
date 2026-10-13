//! `/bug`: consent, an optional model-written summary, then upload or a zip archive (upstream
//! `modes/interactive/bug-report.ts`), and the hint after errors that look like pi bugs.

use std::cell::RefCell;
use std::rc::Rc;

use pi_ai::AssistantMessage;
use pi_ai::retry::is_retryable_assistant_error;
use pi_tui::component;
use regex::Regex;
use serde_json::{Map, Value, json};

use super::components::ThemedText;
use super::components::bordered_loader::BorderedLoader;
use super::mode::InteractiveMode;
use crate::agent_session::AgentSession;
use crate::agent_session::auth::request_auth;
use crate::diagnostics::bug_report::{
    BUG_REPORT_DISCLAIMER, BUG_REPORT_TRANSCRIPT_NOTE, ExtensionDescription, ProviderDescription,
};
use crate::diagnostics::{
    BUG_REPORT_CUSTOM_ENTRY_TYPE, BugReportBundle, BugReportMetadataOptions, BugReportSessionEntryData,
    BugReportSummaryOptions, UploadBugReportOptions, bug_report_archive_file_name, clear_crash_log,
    collect_bug_report_diagnostics, collect_bug_report_metadata, crash_log_path, generate_bug_report_summary,
    radius_gateway_url, read_crash_log, upload_bug_report, write_bug_report_archive,
};
use crate::export::ExportTool;
use crate::export::share::create_share_trailing_entries;
use crate::resources::brand::APP_NAME;
use crate::resources::source_info::{SourceOrigin, SourceScope};
use crate::theme::{ThemeColor, theme};

const RADIUS_PROVIDER_ID: &str = "radius";

struct BugOptions {
    hint: Option<String>,
    include_session: bool,
    include_summary: bool,
    upload: bool,
}

fn model_name(session: &AgentSession) -> String {
    session.model().map_or_else(|| "the current model".to_string(), |model| model.name)
}

impl InteractiveMode {
    /// A one-time hint after an error that is neither retryable nor an abort.
    pub(super) fn maybe_suggest_bug_report(&self, message: &AssistantMessage) {
        if message.stop_reason != pi_ai::StopReason::Error || is_retryable_assistant_error(message) {
            return;
        }
        let aborted = Regex::new(r"(?i)\b(?:abort(?:ed)?|cancel(?:l?ed)?)\b").expect("valid regex");
        if aborted.is_match(message.error_message.as_deref().unwrap_or("")) || self.state.borrow().bug_hint_shown {
            return;
        }
        self.state.borrow_mut().bug_hint_shown = true;
        let pad = self.state.borrow().output_pad;
        let text = format!("If this looks like a {APP_NAME} bug, /bug sends a report to the developers.");
        self.add_to_chat(component(ThemedText::new(move || theme().fg(ThemeColor::Muted, &text), pad, 0)));
        self.request_render();
    }

    /// A choice with a description; `None` when dismissed.
    async fn bug_choice(&self, title: &str, options: &[&str], description: &str) -> Option<String> {
        let options = options.iter().map(|option| option.to_string()).collect();
        self.ask_choice(&format!("{title}\n{description}"), options).await.ok().flatten()
    }

    async fn bug_options(&self, hint: Option<String>) -> Option<BugOptions> {
        let title = format!("Report a bug\n{BUG_REPORT_DISCLAIMER}\n\nWhat went wrong? (optional)");
        let hint = self.ask_editor(&title, hint).await.ok().flatten()?;
        let transcript = self
            .bug_choice(
                "Include the session transcript?",
                &["Yes, include the transcript", "No"],
                BUG_REPORT_TRANSCRIPT_NOTE,
            )
            .await?;
        let include_session = transcript != "No";
        let session = self.session();
        let mut include_summary = false;
        if !include_session {
            let provider = session.model().map_or_else(|| "your provider".to_string(), |model| model.provider);
            let note = format!(
                "The transcript is sent to {provider} with your credentials and tokens. Only the generated summary is attached; the transcript stays on your machine."
            );
            let title = format!("Attach a summary written by {} instead?", model_name(&session));
            include_summary = self.bug_choice(&title, &["Yes, generate a summary", "No"], &note).await? != "No";
        }
        let description = hint.trim().to_string();
        let host = url::Url::parse(&radius_gateway_url()).ok().and_then(|url| url.host_str().map(String::from));
        let summary = if include_summary { format!("written by {}", model_name(&session)) } else { "none".into() };
        let details = format!(
            "Description: {}\nTranscript: {}\nSummary: {summary}\n\nUpload sends the report to {}. Export writes a zip archive to the current directory instead.",
            if description.is_empty() { "none" } else { &description },
            if include_session { "included" } else { "not included" },
            host.unwrap_or_default(),
        );
        let delivery = self.bug_choice("Bug report", &["Upload Report", "Export as Zip", "Cancel"], &details).await?;
        if delivery == "Cancel" {
            return None;
        }
        let hint = (!description.is_empty()).then_some(description);
        Some(BugOptions { hint, include_session, include_summary, upload: delivery == "Upload Report" })
    }

    /// A cancellable loader in the editor slot; returns it and its cancel token.
    fn show_bug_loader(&self, message: &str) -> Rc<RefCell<BorderedLoader>> {
        let loader = Rc::new(RefCell::new(BorderedLoader::new(&self.tui(), message, true)));
        self.show_in_editor_slot(loader.clone(), loader.clone());
        loader
    }

    fn close_bug_loader(&self, loader: &Rc<RefCell<BorderedLoader>>) {
        loader.borrow().dispose();
        self.selector.restore_editor();
    }

    /// `/bug [description]`.
    pub(super) fn handle_bug_command(&self, hint: Option<String>) {
        self.spawn(move |mode| async move { mode.report_bug(hint).await });
    }

    async fn report_bug(self: &Rc<Self>, hint: Option<String>) {
        let Some(options) = self.bug_options(hint).await else { return self.show_status("Bug report cancelled") };
        if options.upload && crate::packages::is_offline_mode_enabled() {
            return self.show_error("Uploading bug reports requires online mode. Use Export as Zip instead.");
        }
        let session = self.session();
        let mut summary = None;
        if options.include_summary {
            let loader = self.show_bug_loader(&format!("Writing summary with {}...", model_name(&session)));
            let cancel = loader.borrow().signal();
            let (writer, hint, token) = (session.clone(), options.hint.clone(), cancel.clone());
            let result = tokio::spawn(async move { write_summary(&writer, hint.as_deref(), token).await }).await;
            self.close_bug_loader(&loader);
            if cancel.is_cancelled() {
                return self.show_status("Bug report cancelled");
            }
            match result {
                Ok(Ok(text)) => summary = Some(text),
                Ok(Err(error)) => return self.show_error(&format!("Failed to write bug report summary: {error}")),
                Err(error) => return self.show_error(&format!("Failed to write bug report summary: {error}")),
            }
        }
        let bundle = self.build_bug_bundle(&session, &options, summary).await;
        if options.upload {
            let Some(failure) = self.upload_bug_bundle(&session, &bundle).await else { return };
            let fallback = self
                .bug_choice(
                    "Upload failed",
                    &["Export as Zip", "Cancel"],
                    &format!("{failure}\n\nExport the report as a zip archive instead?"),
                )
                .await;
            if fallback.as_deref() != Some("Export as Zip") {
                return self.show_status("Bug report cancelled");
            }
        }
        let id = bundle.metadata.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let cwd = std::env::current_dir().unwrap_or_else(|_| session.cwd().to_path_buf());
        let path = cwd.join(bug_report_archive_file_name(&id));
        if let Err(error) = write_bug_report_archive(&bundle, &path) {
            return self.show_error(&format!("Failed to write bug report: {error}"));
        }
        self.record_bug_report(&session, &bundle, "zip", Some(path.display().to_string()));
        self.show_status(&format!("Bug report exported to: {}\nReport ID: {id}", path.display()));
    }

    /// Uploads `bundle`; `None` when done or cancelled, else the failure.
    async fn upload_bug_bundle(
        &self,
        session: &std::sync::Arc<AgentSession>,
        bundle: &BugReportBundle,
    ) -> Option<String> {
        let loader = self.show_bug_loader("Uploading bug report...");
        let cancel = loader.borrow().signal();
        let (runtime, upload, token) = (session.model_runtime().clone(), bundle.clone(), cancel.clone());
        let has_radius = runtime.registry().provider_name(RADIUS_PROVIDER_ID).is_some();
        let result = tokio::spawn(async move {
            let token_value = if has_radius { runtime.api_key_for(RADIUS_PROVIDER_ID).await } else { None };
            let options = UploadBugReportOptions { token: token_value, ..Default::default() };
            upload_bug_report(&upload, &options, &token).await
        })
        .await;
        self.close_bug_loader(&loader);
        if cancel.is_cancelled() {
            self.show_status("Bug report cancelled");
            return None;
        }
        match result {
            Ok(Ok(id)) => {
                self.record_bug_report(session, bundle, "upload", None);
                self.show_status(&format!("Bug report uploaded. Report ID: {id}"));
                None
            }
            Ok(Err(error)) => Some(error),
            Err(error) => Some(error.to_string()),
        }
    }

    fn record_bug_report(
        &self,
        session: &AgentSession,
        bundle: &BugReportBundle,
        delivery: &str,
        path: Option<String>,
    ) {
        let data = BugReportSessionEntryData::from_bundle(bundle, delivery, path);
        let value = serde_json::to_value(&data).unwrap_or_default();
        let _ = session.session_manager().append_custom_entry(BUG_REPORT_CUSTOM_ENTRY_TYPE, Some(value));
        let crashes = bundle.diagnostics.get("crashes").and_then(Value::as_array).is_some_and(|list| !list.is_empty());
        if crashes {
            clear_crash_log(&crash_log_path(&self.agent_dir));
        }
    }

    async fn build_bug_bundle(
        &self,
        session: &std::sync::Arc<AgentSession>,
        options: &BugOptions,
        summary: Option<String>,
    ) -> BugReportBundle {
        let services = session.services().clone();
        let (extensions, extension_errors) = {
            let loader = services.resources();
            let resources = loader.resources();
            let extensions: Vec<ExtensionDescription> = resources
                .extensions
                .iter()
                .map(|entry| ExtensionDescription {
                    path: entry.path.display().to_string(),
                    source: entry.source_info.source.clone(),
                    scope: match entry.source_info.scope {
                        SourceScope::User => "user",
                        SourceScope::Project => "project",
                        SourceScope::Temporary => "temporary",
                    }
                    .into(),
                    origin: if entry.source_info.origin == SourceOrigin::Package { "package" } else { "top-level" }
                        .into(),
                    hidden: false,
                })
                .collect();
            let errors = resources
                .extension_diagnostics
                .iter()
                .map(|diagnostic| (diagnostic.path.clone().unwrap_or_default(), diagnostic.message.clone()))
                .collect();
            (extensions, errors)
        };
        let model = session.model();
        let provider = match &model {
            Some(model) => Some(describe_provider(session, model).await),
            None => None,
        };
        let settings = session.settings();
        let metadata = collect_bug_report_metadata(&BugReportMetadataOptions {
            hint: options.hint.clone(),
            session_id: session.session_id(),
            cwd: session.cwd().display().to_string(),
            include_session: options.include_session,
            include_summary: summary.is_some(),
            message_count: session.messages().len(),
            model,
            provider,
            thinking_level: session.thinking_level().as_str().to_string(),
            extensions,
            extension_errors,
            global_settings: serde_json::to_value(settings.global_settings()).unwrap_or_default(),
            project_settings: serde_json::to_value(settings.project_settings()).unwrap_or_default(),
            ..Default::default()
        });
        let crashes = read_crash_log(&crash_log_path(&self.agent_dir));
        let session_jsonl = options.include_session.then(|| {
            let tools: Vec<ExportTool> = session
                .state()
                .tools
                .iter()
                .map(|tool| ExportTool {
                    name: tool.name().to_string(),
                    description: Some(tool.description().to_string()),
                    parameters: tool.parameters(),
                })
                .collect();
            let prompt = session.system_prompt();
            let trailing = move |parent: Option<&str>, timestamp: &str| -> Vec<Map<String, Value>> {
                create_share_trailing_entries(&prompt, &tools, parent, timestamp)
                    .into_iter()
                    .filter_map(|entry| entry.as_object().cloned())
                    .collect()
            };
            session.session_manager().serialize_branch(Some(&trailing))
        });
        let diagnostics = {
            let manager = session.session_manager();
            collect_bug_report_diagnostics(&session.session_id(), manager.entries(), &crashes)
        };
        BugReportBundle { metadata, diagnostics, session_jsonl, summary }
    }
}

/// The report's provider block: where requests go and how auth is configured, without secrets.
async fn describe_provider(session: &AgentSession, model: &pi_ai::Model) -> ProviderDescription {
    let runtime = session.model_runtime().clone();
    let provider = model.provider.clone();
    let name = runtime.registry().provider_name(&provider).map_or_else(|| provider.clone(), str::to_string);
    let check = runtime.resolver().check(&provider).await.ok().flatten();
    let using_oauth = runtime.is_using_oauth(&provider).await;
    let mut auth_types = Vec::new();
    if runtime.resolver().oauth(&provider).is_some() {
        auth_types.push("oauth".to_string());
    }
    if check.as_ref().is_some_and(|check| check.auth_type == pi_ai::auth::AuthType::ApiKey) {
        auth_types.push("api_key".to_string());
    }
    let auth_status = match &check {
        Some(check) => json!({ "configured": true, "source": check.source }),
        None => json!({ "configured": false }),
    };
    ProviderDescription {
        id: provider,
        name,
        base_url: Some(model.base_url.clone()),
        header_names: model.headers.keys().cloned().collect(),
        auth_types,
        auth_status,
        using_oauth,
        registered_by_extension: false,
    }
}

/// Asks the session model for a summary of the conversation.
async fn write_summary(
    session: &AgentSession,
    hint: Option<&str>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<String, String> {
    let model = session.model().ok_or("No model selected")?;
    let auth = request_auth(session.model_runtime(), &model).await?;
    let messages = session.messages();
    generate_bug_report_summary(BugReportSummaryOptions {
        messages: &messages,
        hint,
        model: Some(&auth.model),
        api_key: auth.auth.api_key.clone(),
        headers: auth.auth.headers.clone(),
        env: auth.env.clone().unwrap_or_default(),
        cancel,
        thinking_level: Some(session.thinking_level()),
        session_id: Some(session.session_id()),
        ..Default::default()
    })
    .await
}
