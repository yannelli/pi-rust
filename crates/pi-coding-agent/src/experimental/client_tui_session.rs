//! Session handling of upstream `client-tui.ts`: Session selection, the presentation bridge facet, the
//! transcript lane, Radius recovery, and prompts and slash commands.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use pi_chord::facets::{Facet, combine_facet_loaders, define_facet};
use pi_chord::state::StateListener;
use pi_chord::{BACKGROUND_CONTEXT, ChordError, ChordResult, Context, Json};
use pi_tui::autocomplete::{AutocompleteItem, CombinedAutocompleteProvider, CommandEntry, SlashCommand};
use pi_tui::components::select_list::SelectItem;
use tokio::sync::oneshot;

use super::cli::{ClientCommand, TransportAddress};
use super::client::resolve_path;
use super::client_runtime::open_services;
use super::client_tui::ExperimentalClientTui;
use super::client_tui_chat::{ExperimentalChatView, live_of};
use super::client_tui_parts::{ActiveServices, ClientTuiServer, FacetState, Lane, Shared};
use super::plugins::bundled::create_presentation_facet_loaders;
use super::services::agent_controller::{
    AGENT_CONTROLLER, AgentController, AgentControllerClient, AgentOperationResponse, AgentPromptRequest,
    AgentQueueResponse,
};
use super::services::connection::{ServerConnectionState, SessionAttachmentState, state_value};
use super::services::plugins::{
    PRESENTATION_PLUGINS, PrepareSessionRequest, PresentationPlugins, PresentationPluginsClient,
};
use super::services::presentation_ui::{PRESENTATION_UI, PresentationSelectItem, PresentationUi};
use super::services::sessions::{
    SESSION_DIRECTORY, SESSION_MANAGEMENT, SessionCreateOptions, SessionManagement, SessionManagementClient,
    SessionSummary, session_directory_state,
};
use super::services::slash_commands::{SLASH_COMMANDS, SlashCommandRunResult};
use super::services::transcript::TRANSCRIPT;

pub(super) struct PreparedClientSession {
    pub server: ClientTuiServer,
    pub summary: SessionSummary,
    pub presentation_plugins: Json,
}

struct Candidate<'a> {
    server: &'a ClientTuiServer,
    management: SessionManagementClient,
    plugins: PresentationPluginsClient,
    sessions: Vec<SessionSummary>,
}

fn require_single<'a, 'b>(candidates: &'b [Candidate<'a>]) -> ChordResult<&'b Candidate<'a>> {
    match candidates {
        [only] => Ok(only),
        _ => Err(ChordError::error("Starting a Session requires exactly one server")),
    }
}

/// Select, create, or continue the Session, prepare its presentation plugins, and attach to it.
pub(super) async fn prepare_client_session(
    command: &ClientCommand,
    servers: &[ClientTuiServer],
) -> ChordResult<PreparedClientSession> {
    let mut opened = Vec::new();
    for server in servers {
        let services = [&*SESSION_DIRECTORY, &*SESSION_MANAGEMENT, &*PRESENTATION_PLUGINS];
        opened.push((server, open_services(server.server.as_ref(), &services)?));
    }
    let result = async {
        let mut candidates = Vec::new();
        for (server, services) in &opened {
            services.ready(&BACKGROUND_CONTEXT).await?;
            let directory = services.use_service(&SESSION_DIRECTORY)?;
            candidates.push(Candidate {
                server,
                management: SessionManagementClient(services.use_service(&SESSION_MANAGEMENT)?),
                plugins: PresentationPluginsClient(services.use_service(&PRESENTATION_PLUGINS)?),
                sessions: session_directory_state(&directory)?.map(|state| state.sessions).unwrap_or_default(),
            });
        }
        let mut selected: Option<(&Candidate, SessionSummary)> = None;
        if let Some(session_id) = &command.session_id {
            let matches: Vec<(&Candidate, SessionSummary)> = candidates
                .iter()
                .flat_map(|candidate| {
                    let found = candidate.sessions.iter().filter(|session| &session.session_id == session_id);
                    found.map(move |summary| (candidate, summary.clone()))
                })
                .collect();
            if matches.len() > 1 {
                return Err(ChordError::error(format!("Session {session_id} is available from more than one server")));
            }
            selected = matches.into_iter().next();
            if selected.is_none() {
                if matches!(command.connect, Some(TransportAddress::Radius { .. })) {
                    return Err(ChordError::error(format!("Remote server does not contain Session {session_id}")));
                }
                let candidate = require_single(&candidates)?;
                let options = SessionCreateOptions { id: Some(session_id.clone()) };
                selected = Some((candidate, candidate.management.create(options, &BACKGROUND_CONTEXT).await?));
            }
        } else if command.continue_session || command.resume {
            let mut all: Vec<(&Candidate, SessionSummary)> = candidates
                .iter()
                .flat_map(|candidate| candidate.sessions.iter().map(move |summary| (candidate, summary.clone())))
                .collect();
            all.sort_by(|(_, left), (_, right)| {
                right
                    .created_at
                    .total_cmp(&left.created_at)
                    .then_with(|| left.server_id.cmp(&right.server_id))
                    .then_with(|| left.session_id.cmp(&right.session_id))
            });
            selected = all.into_iter().next();
        }
        let (candidate, summary) = match selected {
            Some(selected) => selected,
            None => {
                let candidate = require_single(&candidates)?;
                (candidate, candidate.management.create(SessionCreateOptions::default(), &BACKGROUND_CONTEXT).await?)
            }
        };
        let package_paths =
            command.plugin_packages.as_ref().map(|paths| paths.iter().map(|path| resolve_path(path)).collect());
        let request = PrepareSessionRequest { session_id: summary.session_id.clone(), package_paths };
        let presentation_plugins = candidate.plugins.prepare_session(request, &BACKGROUND_CONTEXT).await?;
        candidate.management.attach(summary.session_id.clone(), &BACKGROUND_CONTEXT).await?;
        (candidate.server.when_attached)(summary.session_id.clone()).await?;
        Ok(PreparedClientSession { server: candidate.server.clone(), summary, presentation_plugins })
    }
    .await;
    for (_, services) in &opened {
        let _ = services.dispose(&BACKGROUND_CONTEXT).await;
    }
    result
}

/// Swap in the presentation plugins of `data`, then dispose the retired generation.
pub(super) async fn reload_presentation(facets: Arc<FacetState>, data: Json) -> ChordResult<()> {
    let mut current = facets.presentation.lock().await;
    let candidate = combine_facet_loaders(create_presentation_facet_loaders(&data)?).load().await?;
    let Some(host) = facets.host.get() else { return Err(ChordError::error("TUI facets are not started")) };
    if let Err(error) = host.reload(candidate.facets.clone()).await {
        let cleanup = candidate.dispose().await.err().into_iter().collect();
        return Err(ChordError::with_cleanup(error, cleanup, "TUI plugin reload and cleanup failed"));
    }
    match current.replace(candidate) {
        Some(retired) => retired.dispose().await,
        None => Ok(()),
    }
}

struct BridgeUi(Arc<Shared>);

#[async_trait]
impl PresentationUi for BridgeUi {
    async fn select(
        &self,
        title: &str,
        items: Vec<PresentationSelectItem>,
        selected_value: Option<String>,
        _context: &Context,
    ) -> ChordResult<Option<String>> {
        let (reply, answer) = oneshot::channel();
        let title = title.to_string();
        let items = items
            .into_iter()
            .map(|item| {
                let select = SelectItem::new(&item.value, &item.label);
                match &item.description {
                    Some(description) => select.with_description(description),
                    None => select,
                }
            })
            .collect();
        self.0.post(move |ui| ui.begin_select(title, items, selected_value, reply));
        answer.await.unwrap_or(Ok(None))
    }

    fn show_status(&self, message: &str, _context: &Context) {
        let message = message.to_string();
        self.0.post(move |ui| ui.set_status(message));
    }
}

/// Upstream `@pi/presentation-bridge`: the presentation UI, and the selected Session's services for the TUI.
pub(super) fn bridge_facet(shared: Arc<Shared>, server: ClientTuiServer) -> Arc<dyn Facet> {
    static TOKENS: AtomicU64 = AtomicU64::new(1);
    define_facet("@pi/presentation-bridge", move |env| {
        env.provide_local(&PRESENTATION_UI, Arc::new(BridgeUi(shared.clone())) as Arc<dyn PresentationUi>)?;
        let commands = env.use_local(&SLASH_COMMANDS)?;
        let controller = env.use_service(&AGENT_CONTROLLER)?;
        let transcript = env.use_service(&TRANSCRIPT)?;
        let (env, shared, server) = (env.clone(), shared.clone(), server.clone());
        env.clone().on_activate(move || async move {
            let commands = commands.get()?;
            let token = TOKENS.fetch_add(1, Ordering::SeqCst);
            let services = ActiveServices {
                token,
                server_id: server.server_id.clone(),
                transcript,
                commands: (*commands).clone(),
                controller: AgentControllerClient(controller),
            };
            {
                let mut active = crate::experimental::process::lock(&shared.services);
                if active.is_some() {
                    return Err(ChordError::error("Presentation services are already active"));
                }
                *active = Some(services);
            }
            let owner = shared.clone();
            env.own(move || {
                let mut active = crate::experimental::process::lock(&owner.services);
                if active.as_ref().is_some_and(|active| active.token == token) {
                    *active = None;
                }
                Ok(())
            })?;
            let changed = shared.clone();
            let unsubscribe = commands.subscribe(Arc::new(move |_| changed.post(|ui| ui.update_autocomplete())));
            env.own(move || {
                unsubscribe();
                Ok(())
            })?;
            if server.radius {
                let (connections, server_id) = (shared.clone(), server.server_id.clone());
                let listener = StateListener::sync(move |value, _, _| {
                    let (value, server_id) = (value.clone(), server_id.clone());
                    connections.post(move |ui| ui.handle_connection_state(&server_id, value));
                    Ok(())
                });
                let unsubscribe = server.connection.subscribe(listener);
                env.own(move || unsubscribe.close())?;
                let (attachments, server_id) = (shared.clone(), server.server_id.clone());
                let listener = StateListener::sync(move |value, _, _| {
                    let (value, server_id) = (value.clone(), server_id.clone());
                    attachments.post(move |ui| ui.handle_attachment_state(&server_id, value));
                    Ok(())
                });
                let unsubscribe = server.attachment.subscribe(listener);
                env.own(move || unsubscribe.close())?;
            }
            Ok(())
        })
    })
}

fn message(error: &ChordError) -> String {
    error.to_string()
}

impl ExperimentalClientTui {
    pub(super) fn update_autocomplete(&self) {
        let commands = self.shared.services().map(|services| services.commands.list()).unwrap_or_default();
        let entries = commands
            .into_iter()
            .map(|command| {
                let mut entry = SlashCommand::new(&command.name, command.description.as_deref());
                entry.argument_hint = command.argument_hint.clone();
                entry.get_argument_completions = command.get_argument_completions.clone().map(|complete| {
                    let complete: pi_tui::autocomplete::ArgumentCompleter = Rc::new(move |prefix: &str| {
                        let items = complete(prefix).map(|items| {
                            let item = |completion: super::services::slash_commands::SlashCommandCompletion| {
                                let item = AutocompleteItem::new(completion.value, completion.label);
                                match completion.description {
                                    Some(description) => item.with_description(description),
                                    None => item,
                                }
                            };
                            items.into_iter().map(item).collect()
                        });
                        Box::pin(async move { items })
                    });
                    complete
                });
                CommandEntry::Command(entry)
            })
            .collect();
        let cwd = self.cwd.display().to_string();
        self.editor.set_autocomplete_provider(Rc::new(CombinedAutocompleteProvider::new(entries, cwd, None)));
        (self.request_render)();
    }

    pub(super) fn handle_connection_state(&self, server_id: &str, state: Json) {
        let Ok(state) = state_value::<ServerConnectionState>(state) else { return };
        if self.state.borrow().closed || self.state.borrow().selected_server_id.as_deref() != Some(server_id) {
            return;
        }
        let status = match state {
            ServerConnectionState::Connected { .. } => {
                if self.state.borrow().lane.is_none() {
                    let mut ui = self.state.borrow_mut();
                    (ui.busy, ui.status) = (true, "Reattaching Session…".into());
                    drop(ui);
                    self.rebuild();
                }
                return;
            }
            ServerConnectionState::Connecting { .. } => "Reconnecting to Radius…",
            ServerConnectionState::Disconnected { .. } => "Radius disconnected; retrying…",
        };
        {
            let mut ui = self.state.borrow_mut();
            (ui.busy, ui.status) = (true, status.into());
        }
        self.close_lane();
        self.rebuild();
    }

    pub(super) fn handle_attachment_state(&self, server_id: &str, state: Json) {
        let Ok(state) = state_value::<SessionAttachmentState>(state) else { return };
        let (closed, selected, session) = {
            let ui = self.state.borrow();
            (ui.closed, ui.selected_server_id.clone(), ui.session_id.clone())
        };
        if closed || selected.as_deref() != Some(server_id) || session.is_none() {
            return;
        }
        match state {
            SessionAttachmentState::Attached { session_id } if Some(&session_id) == session.as_ref() => {
                let opened = if self.state.borrow().lane.is_none() { self.open_lane() } else { Ok(()) };
                let mut ui = self.state.borrow_mut();
                ui.busy = opened.is_err();
                ui.status =
                    opened.err().map(|error| format!("Reconnect error: {}", message(&error))).unwrap_or_default();
            }
            SessionAttachmentState::Attaching { session_id } if Some(&session_id) == session.as_ref() => {
                let mut ui = self.state.borrow_mut();
                ui.busy = true;
                ui.status = "Reattaching Session…".into();
            }
            _ => return,
        }
        self.rebuild();
    }

    pub(super) fn open_lane(&self) -> ChordResult<()> {
        self.close_lane();
        let Some(services) = self.shared.services() else {
            return Err(ChordError::error("No Session Transcript service is available"));
        };
        let view = Rc::new(ExperimentalChatView::new(&self.tui, self.cwd.clone()));
        self.document.borrow_mut().add_child(self.heading.clone());
        self.document.borrow_mut().add_child(view.transcript.clone());
        self.pending.borrow_mut().add_child(view.pending_messages.clone());
        let generation = {
            let mut state = self.state.borrow_mut();
            state.generation += 1;
            state.generation
        };
        let state = services.transcript.state("state")?;
        let shared = self.shared.clone();
        let unsubscribe = state.subscribe(StateListener::sync(move |value, _, _| {
            let value = value.to_serde();
            shared.post(move |ui| ui.apply_transcript(generation, &value));
            Ok(())
        }))?;
        self.state.borrow_mut().lane = Some(Lane { generation, unsubscribe, view });
        if state.value()?.is_none() {
            self.close_lane();
            return Err(ChordError::error("Transcript has no initialized view"));
        }
        Ok(())
    }

    fn apply_transcript(&self, generation: u64, value: &serde_json::Value) {
        let view = match &self.state.borrow().lane {
            Some(lane) if lane.generation == generation => lane.view.clone(),
            _ => return,
        };
        view.apply(value);
        self.rebuild();
    }

    pub(super) fn close_lane(&self) {
        if let Some(lane) = self.state.borrow_mut().lane.take() {
            let _ = lane.unsubscribe.close();
            lane.view.dispose();
        }
        self.document.borrow_mut().clear();
        self.pending.borrow_mut().clear();
        self.status.borrow_mut().clear();
    }

    pub(super) fn run_prompt(self: &Rc<Self>, text: &str) {
        let prompt = text.trim();
        if prompt.is_empty() {
            return;
        }
        if let Some(command) = prompt.strip_prefix('/') {
            let (name, args) = command.split_once(' ').unwrap_or((command, ""));
            self.execute_slash_command(name, args.trim());
            return;
        }
        self.editor.set_text("");
        self.submit_prompt(prompt.to_string());
    }

    /// Run a future on the runtime and apply its outcome on the UI thread.
    fn spawn<T: Send + 'static>(
        &self,
        work: impl Future<Output = ChordResult<T>> + Send + 'static,
        done: impl FnOnce(&Rc<Self>, T) + Send + 'static,
    ) {
        let shared = self.shared.clone();
        tokio::spawn(async move {
            match work.await {
                Ok(value) => shared.post(move |ui| done(ui, value)),
                Err(error) => shared.post(move |ui| ui.set_status(format!("Error: {}", message(&error)))),
            }
        });
    }

    pub(super) fn execute_slash_command(self: &Rc<Self>, name: &str, args: &str) {
        let commands = self.shared.services().map(|services| services.commands.list()).unwrap_or_default();
        let command = commands.into_iter().find(|candidate| candidate.name == name);
        self.editor.set_text("");
        let Some(command) = command else {
            return self.set_status(format!("Unknown slash command: /{name}"));
        };
        self.spawn((command.run)(args.to_string(), BACKGROUND_CONTEXT.clone()), |ui, result| match result {
            Some(SlashCommandRunResult::Queue(response)) => ui.report_queue(response),
            Some(SlashCommandRunResult::Operation(response)) => ui.report_operation(response),
            None => {}
        });
    }

    fn controller(&self) -> Option<AgentControllerClient> {
        self.shared.services().map(|services| services.controller)
    }

    fn submit_prompt(self: &Rc<Self>, prompt: String) {
        let Some(controller) = self.controller() else {
            return self.set_status("Error: No Session AgentController service is available".into());
        };
        let running = self.conversation_view().is_some_and(|view| !live_of(&view)["run"].is_null());
        let status = if running { "Queueing steering message…" } else { "Running turn…" };
        self.set_status(status.into());
        let request = AgentPromptRequest::text(prompt);
        if running {
            let work = async move { controller.steer(request, &BACKGROUND_CONTEXT).await };
            self.spawn(work, |ui, response| ui.report_queue(response));
        } else {
            let work = async move { controller.prompt(request, &BACKGROUND_CONTEXT).await };
            self.spawn(work, |ui, response| ui.report_operation(response));
        }
    }

    pub(super) fn queue_follow_up(self: &Rc<Self>, text: String) {
        let Some(controller) = self.controller() else { return };
        self.set_status("Queueing follow-up…".into());
        let work = async move { controller.follow_up(AgentPromptRequest::text(text), &BACKGROUND_CONTEXT).await };
        self.spawn(work, |ui, response| ui.report_queue(response));
    }

    fn report_operation(&self, response: AgentOperationResponse) {
        let rejected = |error: Option<super::services::agent_controller::AgentOperationError>| {
            format!("Operation rejected: {}", error.map(|error| error.message).unwrap_or_default())
        };
        self.set_status(if response.accepted { String::new() } else { rejected(response.error) });
    }

    fn report_queue(&self, response: AgentQueueResponse) {
        let status = match response.accepted {
            true => format!("Queued {}.", response.entry_id.unwrap_or_default()),
            false => format!("Message rejected: {}", response.error.map(|error| error.message).unwrap_or_default()),
        };
        self.set_status(status);
    }

    pub(super) fn interrupt(self: &Rc<Self>) {
        let running = self.conversation_view().is_some_and(|view| !live_of(&view)["run"].is_null());
        let Some(controller) = self.controller().filter(|_| running) else { return };
        self.set_status("Aborting…".into());
        self.spawn(async move { controller.abort(&BACKGROUND_CONTEXT).await }, |ui, ()| {
            if ui.state.borrow().status == "Aborting…" {
                ui.set_status(String::new());
            }
        });
    }
}
