//! Port of upstream `experimental/client-tui.ts`: the service-only presentation driven by the replicated
//! transcript. Components live on the UI thread; service callbacks post tasks to it over a channel.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

use pi_chord::facets::{FacetLoader, FacetOptions, LoadedFacets, combine_facet_loaders, create_facet_host};
use pi_chord::{BoxFuture, ChordError, ChordResult, Json};
use pi_tui::components::select_list::{SelectItem, SelectListLayoutOptions};
use pi_tui::components::{ScrollFollow, ScrollOverscroll, StackOptions};
use pi_tui::layout_node::StackBasis;
use pi_tui::{
    Component, ComponentRef, Container, EditorOptions, ScrollView, ScrollViewOptions, SelectList, Text, Tui, VStack,
    component, get_keybindings,
};
use tokio::sync::{mpsc, oneshot};

use super::cli::ClientCommand;
use super::client_tui_parts::{
    ClientTuiServer, FacetState, InputAdapter, Selection, Shared, UiState, UiTask, stack_entry,
};
use super::client_tui_session::{PreparedClientSession, bridge_facet, prepare_client_session, reload_presentation};
use super::plugins::bundled::create_presentation_facet_loaders;
use super::services::slash_commands::{
    SlashCommandRegistry, create_built_in_slash_commands_facet, create_slash_commands_runtime_facet,
};
use crate::interactive::components::custom_editor::{CustomEditor, EditorHandle};
use crate::theme::{ThemeColor, get_editor_theme, get_select_list_theme, theme};

pub struct ExperimentalClientTui {
    pub(super) tui: Tui,
    pub(super) cwd: PathBuf,
    pub(super) shared: Arc<Shared>,
    pub(super) facets: Arc<FacetState>,
    shared_facets: Arc<LoadedFacets>,
    pub(super) request_render: Rc<dyn Fn()>,
    pub(super) finish: Rc<dyn Fn()>,
    pub(super) document: Rc<RefCell<Container>>,
    pub(super) heading: Rc<RefCell<Text>>,
    pub(super) pending: Rc<RefCell<Container>>,
    pub(super) status: Rc<RefCell<Container>>,
    editor_container: Rc<RefCell<Container>>,
    footer: Rc<RefCell<Text>>,
    layout_root: ComponentRef,
    pub(super) editor: EditorHandle,
    editor_component: ComponentRef,
    input: ComponentRef,
    pub(super) state: RefCell<UiState>,
}

/// Options of [`ExperimentalClientTui::create`].
pub struct ClientTuiOptions {
    pub command: ClientCommand,
    pub tui: Tui,
    pub servers: Vec<ClientTuiServer>,
    pub facet_loader: Option<Arc<dyn FacetLoader>>,
    pub request_render: Rc<dyn Fn()>,
    pub finish: Rc<dyn Fn()>,
}

impl ExperimentalClientTui {
    /// Prepare the selected Session, start the facets, and open its transcript. Must run inside a `LocalSet`.
    pub async fn create(options: ClientTuiOptions) -> ChordResult<Rc<Self>> {
        let prepared = prepare_client_session(&options.command, &options.servers).await?;
        let shared_loader = combine_facet_loaders(options.facet_loader.into_iter().collect());
        let shared_facets = Arc::new(shared_loader.load().await?);
        let (events, receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared { events, services: Mutex::default() });
        let ui = Self::new(options.tui, shared, shared_facets, options.request_render, options.finish);
        ui.start_pump(receiver);
        let started = async {
            ui.start(&prepared).await?;
            ui.open_prepared_session(&prepared).await
        };
        if let Err(error) = started.await {
            let cleanup = ui.close().await.err().into_iter().collect();
            return Err(ChordError::with_cleanup(error, cleanup, "Experimental TUI startup and cleanup failed"));
        }
        Ok(ui)
    }

    fn new(
        tui: Tui,
        shared: Arc<Shared>,
        shared_facets: Arc<LoadedFacets>,
        request_render: Rc<dyn Fn()>,
        finish: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        Rc::new_cyclic(|me: &Weak<Self>| {
            let editor_options = EditorOptions { padding_x: 1, ..Default::default() };
            let custom_editor = CustomEditor::new(&tui, get_editor_theme(), editor_options, false);
            let editor = custom_editor.handle();
            let editor_component: ComponentRef = component(custom_editor);
            let (document, pending, status) =
                (component(Container::new()), component(Container::new()), component(Container::new()));
            let editor_container = component(Container::new());
            editor_container.borrow_mut().add_child(editor_component.clone());
            let (heading, footer) = (component(Text::new("", 1, 0)), component(Text::new("", 1, 0)));
            let options = ScrollViewOptions {
                follow: ScrollFollow::End,
                primary: true,
                overscroll: ScrollOverscroll::Chain,
                ..ScrollViewOptions::default()
            };
            let dock = VStack::new(
                vec![
                    stack_entry(pending.clone(), None, None, 0),
                    stack_entry(status.clone(), None, None, 0),
                    stack_entry(editor_container.clone(), None, None, 3),
                    stack_entry(footer.clone(), None, None, 0),
                ],
                StackOptions::default(),
            );
            let transcript = component(ScrollView::new(document.clone(), options));
            let layout_root = component(VStack::new(
                vec![
                    stack_entry(transcript, Some(StackBasis::Fixed(0)), Some(1), 1),
                    stack_entry(component(dock), Some(StackBasis::Auto), Some(0), 1),
                ],
                StackOptions::default(),
            ));
            Self::bind_editor(&editor, me, &finish);
            Self {
                tui,
                cwd: std::env::current_dir().unwrap_or_default(),
                shared,
                facets: Arc::default(),
                shared_facets,
                request_render,
                finish,
                document,
                heading,
                pending,
                status,
                editor_container,
                footer,
                layout_root,
                editor,
                editor_component,
                input: component(InputAdapter(me.clone())),
                state: RefCell::new(UiState { status: "Starting Session…".into(), ..UiState::default() }),
            }
        })
    }

    /// Editor callbacks run while the editor is borrowed, so they defer their work to the task channel.
    fn bind_editor(editor: &EditorHandle, me: &Weak<Self>, finish: &Rc<dyn Fn()>) {
        let deferred = |me: &Weak<Self>, task: fn(&Rc<Self>)| {
            let me = me.clone();
            move || {
                if let Some(ui) = me.upgrade() {
                    ui.shared.post(task);
                }
            }
        };
        let submit = me.clone();
        editor.on_submit(move |text: &str| {
            let text = text.to_string();
            if let Some(ui) = submit.upgrade() {
                ui.shared.post(move |ui| ui.run_prompt(&text));
            }
        });
        editor.set_on_escape(Some(Rc::new(deferred(me, |ui| ui.interrupt()))));
        let (exit, clear) = (finish.clone(), finish.clone());
        editor.set_on_ctrl_d(Some(Rc::new(move || exit())));
        editor.on_action("app.clear", move || clear());
        editor.on_action("app.model.select", deferred(me, |ui| ui.execute_slash_command("model", "")));
        let follow = me.clone();
        editor.on_action("app.message.followUp", move || {
            let Some(ui) = follow.upgrade() else { return };
            let text = ui.editor.get_text().trim().to_string();
            if text.is_empty() {
                return;
            }
            ui.editor.set_text("");
            ui.shared.post(move |ui| ui.queue_follow_up(text));
        });
    }

    fn start_pump(self: &Rc<Self>, mut receiver: mpsc::UnboundedReceiver<UiTask>) {
        let me = Rc::downgrade(self);
        let pump = tokio::task::spawn_local(async move {
            while let Some(task) = receiver.recv().await {
                let Some(ui) = me.upgrade() else { break };
                task(&ui);
            }
        });
        self.state.borrow_mut().pump = Some(pump);
    }

    async fn start(self: &Rc<Self>, prepared: &PreparedClientSession) -> ChordResult<()> {
        let loaders = create_presentation_facet_loaders(&prepared.presentation_plugins)?;
        let presentation = combine_facet_loaders(loaders).load().await?;
        let presentation_facets = presentation.facets.clone();
        *self.facets.presentation.lock().await = Some(presentation);
        let facets = self.facets.clone();
        let reload = Arc::new(move |data: Json| -> BoxFuture<ChordResult<()>> {
            Box::pin(reload_presentation(facets.clone(), data))
        });
        let mut all = vec![
            create_slash_commands_runtime_facet(SlashCommandRegistry::new()),
            bridge_facet(self.shared.clone(), prepared.server.clone()),
            create_built_in_slash_commands_facet(reload),
        ];
        all.extend(self.shared_facets.facets.iter().cloned());
        all.extend(presentation_facets);
        let mut options = FacetOptions::new(all);
        options.service_sources = vec![prepared.server.server.clone(), prepared.server.session.clone()];
        let host = create_facet_host(options).await?;
        let _ = self.facets.host.set(Arc::new(host));
        Ok(())
    }

    async fn open_prepared_session(self: &Rc<Self>, prepared: &PreparedClientSession) -> ChordResult<()> {
        let server_id = &prepared.server.server_id;
        let Some(feature) = self.shared.services() else {
            return Err(ChordError::error(format!("No Session service is available for {server_id}")));
        };
        let session_id = prepared.summary.session_id.clone();
        (prepared.server.when_attached)(session_id.clone()).await?;
        {
            let mut state = self.state.borrow_mut();
            state.selected_server_id = Some(feature.server_id.clone());
            state.session_id = Some(session_id);
        }
        self.update_autocomplete();
        self.open_lane()?;
        self.state.borrow_mut().status.clear();
        self.rebuild();
        Ok(())
    }

    /// The component that takes focus and forwards input.
    pub fn input_component(&self) -> ComponentRef {
        self.input.clone()
    }

    pub fn layout_root(&self) -> ComponentRef {
        self.layout_root.clone()
    }

    /// The components the layout root shows, for the TUI's focus and mount checks.
    pub fn children(&self) -> Vec<ComponentRef> {
        let containers = [&self.document, &self.pending, &self.status, &self.editor_container];
        let mut children: Vec<ComponentRef> =
            containers.into_iter().map(|child| child.clone() as ComponentRef).collect();
        children.push(self.footer.clone());
        children
    }

    /// Upstream `render`: the document, pending messages, status, editor, and footer.
    pub fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.document.borrow_mut().render(width);
        lines.extend(self.pending.borrow_mut().render(width));
        lines.extend(self.status.borrow_mut().render(width));
        lines.extend(self.editor_container.borrow_mut().render(width));
        lines.extend(self.footer.borrow_mut().render(width));
        lines
    }

    pub fn handle_input(&self, data: &str) {
        let (busy, selecting) = {
            let state = self.state.borrow();
            (state.busy, state.select_list.clone())
        };
        if busy {
            let keybindings = get_keybindings();
            let exits = self.editor.get_text().is_empty() && keybindings.matches(data, "app.exit");
            if keybindings.matches(data, "app.clear") || exits {
                (self.finish)();
            }
            return;
        }
        match selecting {
            Some(list) => list.borrow_mut().handle_input(data),
            None => {
                self.editor.handle_input(data);
                (self.request_render)();
            }
        }
    }

    pub fn refresh_theme(&self) {
        let lane = self.state.borrow().lane.as_ref().map(|lane| lane.view.clone());
        if let (Some(view), Some(value)) = (lane, self.conversation_view()) {
            view.refresh_theme(&value);
        }
        self.rebuild();
    }

    pub fn show_error(&self, error: &str) {
        self.set_status(format!("Error: {error}"));
    }

    pub(super) fn set_status(&self, status: String) {
        self.state.borrow_mut().status = status;
        self.rebuild();
    }

    /// Stop the presentation and dispose its facets; later calls do nothing.
    pub async fn close(&self) -> ChordResult<()> {
        {
            let mut state = self.state.borrow_mut();
            if state.close_started {
                return Ok(());
            }
            state.close_started = true;
            state.closed = true;
        }
        self.complete_selection(None);
        self.close_lane();
        let mut errors = Vec::new();
        let mut presentation = self.facets.presentation.lock().await;
        if let Some(host) = self.facets.host.get() {
            errors.extend(host.dispose().await.err());
        }
        if let Some(retired) = presentation.take() {
            errors.extend(retired.dispose().await.err());
        }
        drop(presentation);
        errors.extend(self.shared_facets.dispose().await.err());
        if let Some(pump) = self.state.borrow_mut().pump.take() {
            pump.abort();
        }
        ChordError::collect(errors, "Failed to dispose experimental TUI facets")
    }

    pub(super) fn rebuild(&self) {
        let state = self.state.borrow();
        let heading = match (&state.session_id, &state.selected_server_id) {
            (Some(session_id), Some(server_id)) => {
                theme().fg(ThemeColor::Dim, &format!("Server: {server_id}\nSession: {session_id}"))
            }
            _ => String::new(),
        };
        self.heading.borrow_mut().set_text(&heading);
        let mut status = self.status.borrow_mut();
        status.clear();
        if !state.status.is_empty() {
            status.add_child(component(Text::new(&theme().fg(ThemeColor::Dim, &state.status), 1, 0)));
        }
        if let Some(lane) = &state.lane {
            status.add_child(lane.view.status.clone());
        }
        drop(status);
        let busy = state.busy;
        drop(state);
        self.footer.borrow_mut().set_text(&theme().fg(ThemeColor::Dim, &self.footer_text()));
        let mut container = self.editor_container.borrow_mut();
        container.clear();
        let mut state = self.state.borrow_mut();
        let list = state.selection.as_ref().map(|selection| self.selector(selection));
        match list {
            Some((selector, list)) => {
                self.editor_component.borrow_mut().set_focused(false);
                container.add_child(selector);
                state.select_list = Some(list);
            }
            None => {
                state.select_list = None;
                self.editor_component.borrow_mut().set_focused(!busy);
                container.add_child(self.editor_component.clone());
            }
        }
        drop((state, container));
        self.layout_root.borrow_mut().invalidate();
        (self.request_render)();
    }

    fn selector(&self, selection: &Selection) -> (ComponentRef, Rc<RefCell<SelectList>>) {
        let mut selector = Container::new();
        selector.add_child(component(Text::new(&theme().bold(&selection.title), 1, 1)));
        let items = selection.items.clone();
        let visible = items.len().clamp(1, 12);
        let index = items.iter().position(|item| Some(&item.value) == selection.selected_value.as_ref());
        let mut list = SelectList::new(items, visible, get_select_list_theme(), SelectListLayoutOptions::default());
        if let Some(index) = index {
            list.set_selected_index(index);
        }
        let (selected, cancelled) = (self.shared.clone(), self.shared.clone());
        list.on_select = Some(Box::new(move |item: &SelectItem| {
            let value = item.value.clone();
            selected.post(move |ui| ui.complete_selection(Some(value)));
        }));
        list.on_cancel = Some(Box::new(move || cancelled.post(|ui| ui.complete_selection(None))));
        let list = component(list);
        selector.add_child(list.clone());
        (component(selector), list)
    }

    pub(super) fn begin_select(
        &self,
        title: String,
        items: Vec<SelectItem>,
        selected_value: Option<String>,
        reply: oneshot::Sender<ChordResult<Option<String>>>,
    ) {
        let mut state = self.state.borrow_mut();
        if state.closed {
            let _ = reply.send(Ok(None));
            return;
        }
        if state.selection.is_some() {
            let _ = reply.send(Err(ChordError::error("A slash command selector is already active")));
            return;
        }
        state.selection = Some(Selection { title, items, selected_value, reply });
        drop(state);
        self.rebuild();
    }

    pub(super) fn complete_selection(&self, value: Option<String>) {
        let Some(selection) = self.state.borrow_mut().selection.take() else { return };
        let _ = selection.reply.send(Ok(value));
        if !self.state.borrow().closed {
            self.rebuild();
        }
    }

    pub(super) fn conversation_view(&self) -> Option<serde_json::Value> {
        let services = self.shared.services()?;
        services.transcript.state("state").ok()?.value().ok().flatten().map(|value| value.to_serde())
    }

    fn footer_text(&self) -> String {
        let commands = "/model · /thinking · /compact · /reload";
        let Some(view) = self.conversation_view() else { return commands.into() };
        let agent = &view["docs"]["pi.agent"];
        let model = match (agent["model"]["provider"].as_str(), agent["model"]["modelId"].as_str()) {
            (Some(provider), Some(model_id)) => format!("{provider}/{model_id}"),
            _ => "no model".into(),
        };
        let thinking = agent["thinkingLevel"].as_str().unwrap_or("off");
        let entries = view["entries"].as_array().map_or(0, Vec::len);
        format!("{model} · thinking:{thinking} · {entries} entries · {commands}")
    }
}

impl Drop for ExperimentalClientTui {
    fn drop(&mut self) {
        if let Some(pump) = self.state.get_mut().pump.take() {
            pump.abort();
        }
    }
}
