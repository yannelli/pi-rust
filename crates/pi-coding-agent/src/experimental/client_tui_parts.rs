//! Types of the experimental client TUI (upstream `client-tui.ts`): the servers it presents, the state shared
//! with service callbacks, and the UI-thread state.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex, OnceLock};

use pi_chord::facets::{FacetHost, LoadedFacets};
use pi_chord::services::{RemoteServiceSource, ServiceHandle};
use pi_chord::state::MutableReplicatedState;
use pi_chord::{BACKGROUND_CONTEXT, BoxFuture, ChordResult, Unsubscribe};
use pi_tui::autocomplete::{
    ArgumentCompleter, AutocompleteItem, CombinedAutocompleteProvider, CommandEntry, SlashCommand,
};
use pi_tui::components::select_list::SelectItem;
use pi_tui::components::{StackChild, StackEntryOptions};
use pi_tui::layout_node::StackBasis;
use pi_tui::{Component, ComponentRef, SelectList};
use tokio::sync::{mpsc, oneshot};

use super::client_runtime::ClientRuntimeServer;
use super::client_tui::ExperimentalClientTui;
use super::client_tui_chat::ExperimentalChatView;
use super::services::agent_controller::AgentControllerClient;
use super::services::slash_commands::{SlashCommandCompletion, SlashCommandContribution, SlashCommandRegistry};

pub type WhenAttached = Arc<dyn Fn(String) -> BoxFuture<ChordResult<()>> + Send + Sync>;

/// Upstream `ClientTuiServer`: the service sources of one server and the states the TUI follows.
#[derive(Clone)]
pub struct ClientTuiServer {
    pub server_id: String,
    pub radius: bool,
    pub server: Arc<dyn RemoteServiceSource>,
    pub session: Arc<dyn RemoteServiceSource>,
    /// `ServerConnectionState` of the server source.
    pub connection: MutableReplicatedState,
    /// `SessionAttachmentState` of the Session source.
    pub attachment: MutableReplicatedState,
    /// Upstream `session.whenAttached`.
    pub when_attached: WhenAttached,
}

impl ClientTuiServer {
    pub fn from_runtime(server: &ClientRuntimeServer) -> Self {
        let session = server.session.clone();
        Self {
            server_id: server.route.server_id().to_string(),
            radius: server.route.is_radius(),
            server: server.server.clone(),
            session: server.session.clone(),
            connection: server.server.connection.clone(),
            attachment: server.session.attachment().clone(),
            when_attached: Arc::new(move |session_id| {
                let session = session.clone();
                Box::pin(async move { session.when_attached(&session_id, &BACKGROUND_CONTEXT).await })
            }),
        }
    }
}

pub(super) type UiTask = Box<dyn FnOnce(&Rc<ExperimentalClientTui>) + Send>;

/// The services the presentation bridge facet hands to the TUI while it is active.
#[derive(Clone)]
pub(super) struct ActiveServices {
    pub token: u64,
    pub server_id: String,
    pub transcript: ServiceHandle,
    pub commands: SlashCommandRegistry,
    pub controller: AgentControllerClient,
}

/// State shared with service callbacks on other threads.
pub(super) struct Shared {
    pub events: mpsc::UnboundedSender<UiTask>,
    pub services: Mutex<Option<ActiveServices>>,
}

impl Shared {
    pub fn post(&self, task: impl FnOnce(&Rc<ExperimentalClientTui>) + Send + 'static) {
        let _ = self.events.send(Box::new(task));
    }

    pub fn services(&self) -> Option<ActiveServices> {
        crate::experimental::process::lock(&self.services).clone()
    }
}

/// The facet host and the loaded presentation plugins; the async lock serializes reloads.
#[derive(Default)]
pub(super) struct FacetState {
    pub host: OnceLock<Arc<FacetHost>>,
    pub presentation: tokio::sync::Mutex<Option<LoadedFacets>>,
}

pub(super) struct Selection {
    pub title: String,
    pub items: Vec<SelectItem>,
    pub selected_value: Option<String>,
    pub reply: oneshot::Sender<ChordResult<Option<String>>>,
}

pub(super) struct Lane {
    pub generation: u64,
    pub unsubscribe: Unsubscribe,
    pub view: Rc<ExperimentalChatView>,
}

#[derive(Default)]
pub(super) struct UiState {
    pub selection: Option<Selection>,
    pub select_list: Option<Rc<RefCell<SelectList>>>,
    pub selected_server_id: Option<String>,
    pub session_id: Option<String>,
    pub status: String,
    pub busy: bool,
    pub closed: bool,
    pub close_started: bool,
    pub lane: Option<Lane>,
    pub generation: u64,
    pub pump: Option<tokio::task::JoinHandle<()>>,
}

pub(super) fn stack_entry(
    child: ComponentRef,
    basis: Option<StackBasis>,
    grow: Option<usize>,
    min_size: usize,
) -> StackChild {
    let options = StackEntryOptions { basis, grow, shrink: Some(1), min_size: Some(min_size), ..Default::default() };
    StackChild::Entry(child, options)
}

/// Forwards terminal input to the TUI while it has focus.
pub(super) struct InputAdapter(pub Weak<ExperimentalClientTui>);

impl Component for InputAdapter {
    fn render(&mut self, _width: usize) -> Vec<String> {
        Vec::new()
    }
    fn handles_input(&self) -> bool {
        true
    }
    fn handle_input(&mut self, data: &str) {
        if let Some(ui) = self.0.upgrade() {
            ui.handle_input(data);
        }
    }
    fn is_focusable(&self) -> bool {
        true
    }
}

/// The editor's completion of the registered slash commands and their arguments.
pub(super) fn autocomplete_provider(
    commands: Vec<SlashCommandContribution>,
    cwd: String,
) -> CombinedAutocompleteProvider {
    let item = |completion: SlashCommandCompletion| {
        let item = AutocompleteItem::new(completion.value, completion.label);
        match completion.description {
            Some(description) => item.with_description(description),
            None => item,
        }
    };
    let entries = commands
        .into_iter()
        .map(|command| {
            let mut entry = SlashCommand::new(&command.name, command.description.as_deref());
            entry.argument_hint = command.argument_hint.clone();
            entry.get_argument_completions = command.get_argument_completions.clone().map(|complete| {
                let complete: ArgumentCompleter = Rc::new(move |prefix: &str| {
                    let items = complete(prefix).map(|items| items.into_iter().map(item).collect());
                    Box::pin(async move { items })
                });
                complete
            });
            CommandEntry::Command(entry)
        })
        .collect();
    CombinedAutocompleteProvider::new(entries, cwd, None)
}
