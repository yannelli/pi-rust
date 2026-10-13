//! The interactive mode: its state, the renderer it drives, and helpers that schedule work on
//! the UI thread (upstream `interactive-mode.ts` constructor and fields).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pi_ai::{AssistantMessage, ContentBlock};
use pi_tui::{ComponentRef, Container, EditorOptions, ProcessTerminal, TerminalRef, Tui, component, ui_loop};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::InteractiveOptions;
use super::bridge::EventBridge;
use super::components::custom_editor::Action;
use super::components::{
    AssistantMessageComponent, BashExecutionComponent, CustomEditor, EditorHandle, ExpandableText, FooterComponent,
    SharedIndicator, ToolExecutionComponent,
};
use super::footer_data::FooterData;
use super::layout::{Layout, ModeThemeHost, apply_capability_overrides, create_tui, renderer_mode};
use super::selector::SelectorSlot;
use super::status::ProgramStatusReporter;
use crate::agent_session::{AgentSession, AgentSessionRuntime};
use crate::settings::SettingsManager;
use crate::settings::schema::TuiMode;
use crate::theme::{InteractiveThemeController, ThemeControllerOptions, get_editor_theme};

pub(super) const DEFAULT_HIDDEN_THINKING_LABEL: &str = "Thinking...";
pub(super) const DEFAULT_WORKING_MESSAGE: &str = "Working";

/// A prompt for the prompt worker, which runs them one after another.
pub(super) struct PromptRequest {
    pub text: String,
    pub images: Vec<ContentBlock>,
}

/// A message typed while compaction runs, sent once it ends.
#[derive(Clone)]
pub(super) struct CompactionQueued {
    pub text: String,
    pub follow_up: bool,
}

/// The last status line, updated in place by back-to-back status messages.
pub(super) struct LastStatus {
    pub spacer: ComponentRef,
    pub text: ComponentRef,
    pub message: Rc<RefCell<String>>,
}

/// Mutable state of the mode. Borrow it briefly, never across calls into components.
pub(super) struct ModeState {
    pub initialized: bool,
    pub shutting_down: bool,
    pub exit: bool,
    pub last_sigint: Option<Instant>,
    pub last_escape: Option<Instant>,
    pub changelog_markdown: Option<String>,
    pub startup_notices_shown: bool,
    pub last_status: Option<LastStatus>,
    pub streaming_component: Option<Rc<RefCell<AssistantMessageComponent>>>,
    pub streaming_message: Option<AssistantMessage>,
    pub pending_tools: Vec<(String, Rc<RefCell<ToolExecutionComponent>>)>,
    pub boundary_rendered_entries: HashSet<String>,
    pub tool_output_expanded: bool,
    pub hide_thinking_block: bool,
    pub output_pad: usize,
    pub hidden_thinking_label: String,
    pub skill_commands: HashMap<String, PathBuf>,
    pub is_bash_mode: bool,
    pub bash_component: Option<Rc<RefCell<BashExecutionComponent>>>,
    pub pending_bash_components: Vec<Rc<RefCell<BashExecutionComponent>>>,
    pub compaction_escape: Option<Option<Action>>,
    pub retry_escape: Option<Option<Action>>,
    pub compaction_queue: Vec<CompactionQueued>,
    pub active_status: Option<SharedIndicator>,
    pub active_status_embedded: bool,
    pub working_visible: bool,
    pub header: Option<Rc<RefCell<ExpandableText>>>,
    pub bug_hint_shown: bool,
    pub anthropic_warning_shown: bool,
    /// Login options for `/login` completion, refreshed after each login and logout.
    pub login_providers: Vec<crate::interactive_components::login::AuthSelectorProvider>,
    pub main_screen_state: Option<pi_tui::TuiMainScreenRenderState>,
}

impl ModeState {
    fn new(settings: &SettingsManager) -> Self {
        Self {
            initialized: false,
            shutting_down: false,
            exit: false,
            last_sigint: None,
            last_escape: None,
            changelog_markdown: None,
            startup_notices_shown: false,
            last_status: None,
            streaming_component: None,
            streaming_message: None,
            pending_tools: Vec::new(),
            boundary_rendered_entries: HashSet::new(),
            tool_output_expanded: false,
            hide_thinking_block: settings.hide_thinking_block(),
            output_pad: usize::from(settings.output_pad()),
            hidden_thinking_label: DEFAULT_HIDDEN_THINKING_LABEL.to_string(),
            skill_commands: HashMap::new(),
            is_bash_mode: false,
            bash_component: None,
            pending_bash_components: Vec::new(),
            compaction_escape: None,
            retry_escape: None,
            compaction_queue: Vec::new(),
            active_status: None,
            active_status_embedded: false,
            working_visible: true,
            header: None,
            bug_hint_shown: false,
            anthropic_warning_shown: false,
            login_providers: Vec::new(),
            main_screen_state: None,
        }
    }
}

/// The full-screen interactive mode. Lives on the UI thread inside a `tokio::task::LocalSet`.
pub struct InteractiveMode {
    pub(super) weak: Weak<InteractiveMode>,
    pub(super) runtime: Arc<AgentSessionRuntime>,
    pub(super) agent_dir: PathBuf,
    pub(super) options: RefCell<InteractiveOptions>,
    tui: RefCell<Tui>,
    retired_tuis: RefCell<Vec<Tui>>,
    pub(super) terminal: TerminalRef,
    pub(super) uses_process_terminal: bool,
    pub(super) bridge: Arc<EventBridge>,
    pub(super) ui: Layout,
    pub(super) editor: EditorHandle,
    pub(super) selector: Rc<SelectorSlot>,
    pub(super) footer: Rc<RefCell<FooterComponent>>,
    pub(super) footer_data: Rc<RefCell<FooterData>>,
    theme: RefCell<Option<InteractiveThemeController>>,
    pub(super) program_status: RefCell<ProgramStatusReporter>,
    pub(super) state: RefCell<ModeState>,
    prompts: UnboundedSender<PromptRequest>,
    prompt_receiver: RefCell<Option<UnboundedReceiver<PromptRequest>>>,
}

impl InteractiveMode {
    /// Build the mode for `runtime`. Drive it with [`InteractiveMode::run`] inside a `LocalSet`.
    pub fn new(runtime: Arc<AgentSessionRuntime>, mut options: InteractiveOptions) -> Rc<Self> {
        let session = runtime.session();
        let settings = session.settings().clone();
        apply_capability_overrides(&settings);
        let tui_mode = options.tui_mode.unwrap_or_else(|| settings.tui_mode());
        options.tui_mode = Some(tui_mode);
        let agent_dir = options.agent_dir.clone().unwrap_or_else(crate::config::agent_dir);
        let (terminal, uses_process_terminal) = match options.terminal.clone() {
            Some(terminal) => (terminal, false),
            None => (Rc::new(RefCell::new(ProcessTerminal::new())) as TerminalRef, true),
        };
        let (prompts, prompt_receiver) = unbounded_channel();
        let mode = Rc::new_cyclic(|weak: &Weak<InteractiveMode>| {
            let tui = create_tui(tui_mode, terminal.clone(), &settings, &agent_dir, weak.clone());
            tui.set_clear_on_shrink(settings.clear_on_shrink());
            let editor_options = EditorOptions {
                padding_x: settings.editor_padding_x() as usize,
                autocomplete_max_visible: settings.autocomplete_max_visible() as usize,
            };
            let custom_editor = CustomEditor::new(&tui, get_editor_theme(), editor_options, true);
            let editor = custom_editor.handle();
            let editor_component: ComponentRef = component(custom_editor);
            let mut editor_container = Container::new();
            editor_container.add_child(editor_component.clone());
            let editor_container = Rc::new(RefCell::new(editor_container));
            let selector = SelectorSlot::new(tui.clone(), editor_container.clone(), editor_component);
            let footer_data = Rc::new(RefCell::new(FooterData::new(session.cwd())));
            let mut footer = FooterComponent::new(session.clone(), footer_data.clone());
            footer.set_auto_compact_enabled(session.auto_compaction_enabled());
            let footer = Rc::new(RefCell::new(footer));
            let ui = Layout::new(editor_container, &settings);
            ui.footer.borrow_mut().add_child(footer.clone());
            InteractiveMode {
                weak: weak.clone(),
                runtime: runtime.clone(),
                agent_dir: agent_dir.clone(),
                options: RefCell::new(options),
                tui: RefCell::new(tui),
                retired_tuis: RefCell::new(Vec::new()),
                terminal: terminal.clone(),
                uses_process_terminal,
                bridge: EventBridge::new(ui_loop::waker()),
                ui,
                editor,
                selector,
                footer,
                footer_data,
                theme: RefCell::new(None),
                program_status: RefCell::new(ProgramStatusReporter::default()),
                state: RefCell::new(ModeState::new(&settings)),
                prompts,
                prompt_receiver: RefCell::new(Some(prompt_receiver)),
            }
        });
        mode.install_theme_controller();
        mode.install_debug_handler(&mode.tui());
        let bridge = mode.bridge.clone();
        runtime.set_rebind_session(Some(Arc::new(move |session: &Arc<AgentSession>| bridge.rebind(session))));
        mode
    }

    pub(super) fn session(&self) -> Arc<AgentSession> {
        self.runtime.session()
    }

    pub(super) fn settings(&self) -> Arc<SettingsManager> {
        self.session().settings().clone()
    }

    /// The active renderer. It changes when switching between regular and fullscreen mode.
    pub fn tui(&self) -> Tui {
        self.tui.borrow().clone()
    }

    pub(super) fn tui_mode(&self) -> TuiMode {
        self.options.borrow().tui_mode.unwrap_or_default()
    }

    /// Whether the mode finished (`/quit`, Ctrl+D, double Ctrl+C).
    pub fn is_exited(&self) -> bool {
        self.state.borrow().exit
    }

    /// Schedule a render and wake the UI loop, which may wait without a deadline.
    pub(super) fn request_render(&self) {
        self.tui().request_render(false);
        self.bridge.waker().wake();
    }

    pub(super) fn request_full_render(&self) {
        self.tui().request_render(true);
        self.bridge.waker().wake();
    }

    /// Run `f` once the current input or event dispatch returns, when no component is borrowed.
    pub(super) fn defer(&self, f: impl FnOnce(&Rc<InteractiveMode>) + 'static) {
        let weak = self.weak.clone();
        ui_loop::set_timeout(Duration::ZERO, move || {
            if let Some(mode) = weak.upgrade() {
                f(&mode);
            }
        });
        self.bridge.waker().wake();
    }

    /// Run an async task on the UI thread with the mode.
    pub(super) fn spawn<F, Fut>(&self, f: F)
    where
        F: FnOnce(Rc<InteractiveMode>) -> Fut + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let weak = self.weak.clone();
        tokio::task::spawn_local(async move {
            if let Some(mode) = weak.upgrade() {
                f(mode).await;
            }
        });
        self.bridge.waker().wake();
    }

    /// An editor action calling `f` with the mode.
    pub(super) fn action(&self, f: impl Fn(&Rc<InteractiveMode>) + 'static) -> Action {
        let weak = self.weak.clone();
        Rc::new(move || {
            if let Some(mode) = weak.upgrade() {
                f(&mode);
            }
        })
    }

    pub(super) fn queue_prompt(&self, text: String, images: Vec<ContentBlock>) {
        let _ = self.prompts.send(PromptRequest { text, images });
    }

    /// Run queued prompts one at a time on the runtime, reporting failures in the chat.
    pub(super) fn start_prompt_worker(&self) {
        let Some(mut receiver) = self.prompt_receiver.borrow_mut().take() else { return };
        let weak = self.weak.clone();
        tokio::task::spawn_local(async move {
            while let Some(request) = receiver.recv().await {
                let Some(session) = weak.upgrade().map(|mode| mode.session()) else { break };
                let run = tokio::spawn(async move { session.prompt(&request.text, request.images).await });
                let error = match run.await {
                    Ok(Ok(_)) => None,
                    Ok(Err(error)) => Some(format!("{error:#}")),
                    Err(error) => Some(error.to_string()),
                };
                if let (Some(error), Some(mode)) = (error, weak.upgrade()) {
                    mode.show_error(&error);
                }
            }
        });
    }

    fn install_theme_controller(&self) {
        let (settings_source, errors, changed) = (self.weak.clone(), self.weak.clone(), self.weak.clone());
        let selection = self.options.borrow().initial_theme_setting.clone();
        let controller = InteractiveThemeController::new(
            Rc::new(ModeThemeHost(self.weak.clone())),
            ThemeControllerOptions {
                get_theme_setting: Box::new(move || settings_source.upgrade()?.settings().theme_setting()),
                show_error: Box::new(move |message| {
                    if let Some(mode) = errors.upgrade() {
                        mode.show_error(message);
                    }
                }),
                on_changed: Box::new(move || {
                    if let Some(mode) = changed.upgrade() {
                        mode.update_editor_border_color();
                    }
                }),
                initial_theme_setting: selection,
            },
        );
        *self.theme.borrow_mut() = Some(controller);
    }

    pub(super) fn theme_controller(&self) -> Option<InteractiveThemeController> {
        self.theme.borrow().clone()
    }

    fn mount(&self, tui: &Tui, children: &[ComponentRef]) {
        for child in children {
            tui.add_child(child.clone());
        }
        if tui.is_viewport() {
            tui.set_layout_root(Some(self.ui.root.clone()));
        }
    }

    pub(super) fn mount_layout(&self) {
        self.mount(&self.tui(), &self.ui.roots());
    }

    /// Replace the renderer by one for `mode`, keeping the component tree and focus (upstream
    /// `switchTuiMode`). Returns false while an overlay is open.
    pub(super) fn switch_tui_mode(&self, mode: TuiMode, restore_progress: bool, start: bool) -> bool {
        let previous = self.tui();
        if renderer_mode(mode) == previous.mode() {
            return true;
        }
        if previous.has_overlay() {
            return false;
        }
        let children = previous.children();
        let focus = previous.get_focused_component();
        let clear_on_shrink = previous.get_clear_on_shrink();
        if let Some(state) = previous.capture_render_state() {
            self.state.borrow_mut().main_screen_state = Some(state);
        }
        previous.stop(pi_tui::TuiStopOptions { preserve_screen: true });
        previous.set_focus(None);
        previous.clear();
        if previous.is_viewport() {
            previous.set_layout_root(None);
        }
        let settings = self.settings();
        let next = create_tui(mode, self.terminal.clone(), &settings, &self.agent_dir, self.weak.clone());
        next.set_clear_on_shrink(clear_on_shrink);
        self.install_debug_handler(&next);
        let saved = if next.is_viewport() { None } else { self.state.borrow_mut().main_screen_state.take() };
        if let Some(state) = saved {
            next.restore_render_state(state);
        }
        previous.hand_over_to(&next);
        self.retired_tuis.borrow_mut().push(previous);
        *self.tui.borrow_mut() = next.clone();
        self.selector.set_tui(next.clone());
        self.options.borrow_mut().tui_mode = Some(mode);
        self.mount(&next, &children);
        next.invalidate();
        next.set_focus(focus);
        if !start {
            return true;
        }
        next.start();
        if let Some(controller) = self.theme_controller() {
            controller.rebind_tui(Rc::new(ModeThemeHost(self.weak.clone())));
        }
        let session = self.session();
        if restore_progress && settings.show_terminal_progress() && (session.is_streaming() || session.is_compacting())
        {
            self.terminal.borrow_mut().set_progress(true);
        }
        true
    }

    fn install_debug_handler(&self, tui: &Tui) {
        let weak = self.weak.clone();
        tui.set_on_debug(Some(Box::new(move || {
            if let Some(mode) = weak.upgrade() {
                mode.defer(|mode| mode.handle_debug_command());
            }
        })));
    }

    /// Process queued session events.
    pub fn drain_events(&self) {
        for event in self.bridge.drain() {
            self.handle_event(event);
        }
    }

    /// One pass of the UI loop: input, timers, renders, then queued session events.
    pub async fn run_once(&self) -> anyhow::Result<()> {
        ui_loop::run_once_async().await.map_err(|error| anyhow::anyhow!("{error}"))?;
        self.drain_events();
        Ok(())
    }

    /// Set up the UI, send the startup messages, and run until the user quits.
    pub async fn run(self: &Rc<Self>) -> anyhow::Result<()> {
        self.init().await;
        self.start_prompt_worker();
        self.show_startup_warnings();
        self.send_startup_messages();
        while !self.is_exited() {
            if let Err(error) = self.run_once().await {
                self.state.borrow_mut().shutting_down = true;
                self.tui().stop(pi_tui::TuiStopOptions::default());
                return Err(error);
            }
        }
        Ok(())
    }

    fn send_startup_messages(&self) {
        let (message, images, messages) = {
            let mut options = self.options.borrow_mut();
            (
                options.initial_message.take(),
                std::mem::take(&mut options.initial_images),
                std::mem::take(&mut options.initial_messages),
            )
        };
        if let Some(message) = message {
            self.queue_prompt(message, images);
        }
        for message in messages {
            self.queue_prompt(message, Vec::new());
        }
    }
}
