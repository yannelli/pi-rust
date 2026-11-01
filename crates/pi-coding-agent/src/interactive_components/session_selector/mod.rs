//! [`SessionSelector`]: the `/resume` picker (upstream `session-selector.ts`). Lists the current
//! folder's or all sessions with search, sort modes, the named filter, path display, rename, and
//! delete with confirmation. Sessions come from loader callbacks that run on a tokio `LocalSet`.

mod header;
mod list;
mod loaders;
mod loading;
pub mod search;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use futures::future::LocalBoxFuture;
use pi_tui::components::Input;
use pi_tui::{Component, ComponentRef, Container, component, get_keybindings};
use tokio_util::sync::CancellationToken;

pub use self::header::{SessionSelectorHeader, StatusKind, StatusMessage};
pub use self::list::{SessionList, format_session_age};
pub use self::loaders::{all_sessions_loader, project_sessions_loader};
use self::loading::{ActiveLoad, ProgressUpdate};
pub use self::search::{NameFilter, SortMode};
use super::DynamicBorder;
use super::common::{bold, fg, spacer, text};
use super::key_text;
use crate::session::SessionInfo;
use crate::theme::ThemeColor;

/// Which sessions the selector lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionScope {
    #[default]
    Current,
    All,
}

/// Upstream `SessionListProgress`: files loaded, total files, and on periodic updates the
/// sessions loaded so far, sorted by activity.
pub type SessionListProgress = Rc<dyn Fn(usize, usize, Option<&[SessionInfo]>)>;
/// The sessions a loader produces.
pub type SessionsFuture = LocalBoxFuture<'static, anyhow::Result<Vec<SessionInfo>>>;
/// Upstream `SessionsLoader`: called with a progress callback and a cancellation token.
pub type SessionsLoader = Box<dyn FnMut(SessionListProgress, CancellationToken) -> SessionsFuture>;
/// Renames a session file; receives the path and the trimmed new name.
pub type RenameSessionFn = Box<dyn FnMut(PathBuf, String) -> anyhow::Result<()>>;
/// Shared render request callback.
pub type RequestRender = Rc<dyn Fn()>;

/// Optional selector features (upstream constructor `options`).
#[derive(Default)]
pub struct SessionSelectorOptions {
    /// Enables `app.session.rename`.
    pub rename_session: Option<RenameSessionFn>,
    /// Shows the rename hint; defaults to whether renaming is enabled.
    pub show_rename_hint: Option<bool>,
}

/// What the session list asked for; handled once the list returns from `handle_input`.
enum ListEvent {
    Select(PathBuf),
    Cancel,
    Exit,
    ToggleScope,
    ToggleSort,
    ToggleNameFilter,
    TogglePath(bool),
    DeleteConfirmationChange(Option<PathBuf>),
    DeleteSession(PathBuf),
    RenameSession(PathBuf),
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Rename,
}

/// The session selector. Focus the selector itself: it routes keys to the list or the rename
/// input and applies list actions after the list returns.
pub struct SessionSelector {
    this: Weak<RefCell<SessionSelector>>,
    container: Container,
    header: Rc<RefCell<SessionSelectorHeader>>,
    session_list: Rc<RefCell<SessionList>>,
    events: Rc<RefCell<Vec<ListEvent>>>,
    scope: SessionScope,
    sort_mode: SortMode,
    name_filter: NameFilter,
    current_sessions: Option<Vec<SessionInfo>>,
    all_sessions: Option<Vec<SessionInfo>>,
    current_sessions_loader: SessionsLoader,
    all_sessions_loader: SessionsLoader,
    current_load: Option<ActiveLoad>,
    all_load: Option<ActiveLoad>,
    next_load_id: u64,
    progress: Rc<RefCell<Vec<ProgressUpdate>>>,
    request_render: RequestRender,
    rename_session: Option<RenameSessionFn>,
    mode: Mode,
    rename_input: Rc<RefCell<Input>>,
    rename_submitted: Rc<RefCell<Option<String>>>,
    rename_target_path: Option<PathBuf>,
    on_select: Box<dyn FnMut(PathBuf)>,
    on_cancel: Box<dyn FnMut()>,
    on_exit: Box<dyn FnMut()>,
}

fn emit(events: &Rc<RefCell<Vec<ListEvent>>>, event: impl Fn() -> ListEvent + 'static) -> Box<dyn FnMut()> {
    let events = events.clone();
    Box::new(move || events.borrow_mut().push(event()))
}

fn emit_with<T: 'static>(
    events: &Rc<RefCell<Vec<ListEvent>>>,
    event: impl Fn(T) -> ListEvent + 'static,
) -> Box<dyn FnMut(T)> {
    let events = events.clone();
    Box::new(move |value| events.borrow_mut().push(event(value)))
}

impl SessionSelector {
    /// Starts loading the current folder's sessions right away. A loader whose future is ready
    /// on the first poll finishes synchronously; others continue on `spawn_local`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        current_sessions_loader: impl FnMut(SessionListProgress, CancellationToken) -> SessionsFuture + 'static,
        all_sessions_loader: impl FnMut(SessionListProgress, CancellationToken) -> SessionsFuture + 'static,
        on_select: impl FnMut(PathBuf) + 'static,
        on_cancel: impl FnMut() + 'static,
        on_exit: impl FnMut() + 'static,
        request_render: impl Fn() + 'static,
        options: SessionSelectorOptions,
        current_session_file_path: Option<&Path>,
    ) -> Rc<RefCell<Self>> {
        let request_render: RequestRender = Rc::new(request_render);
        let (scope, sort_mode, name_filter) = (SessionScope::Current, SortMode::Threaded, NameFilter::All);
        let can_rename = options.rename_session.is_some();
        let mut header = SessionSelectorHeader::new(scope, sort_mode, name_filter, request_render.clone());
        header.set_show_rename_hint(options.show_rename_hint.unwrap_or(can_rename));
        let events: Rc<RefCell<Vec<ListEvent>>> = Rc::default();
        let mut list = SessionList::new(Vec::new(), false, sort_mode, name_filter, current_session_file_path);
        list.on_select = Some(emit_with(&events, ListEvent::Select));
        list.on_cancel = Some(emit(&events, || ListEvent::Cancel));
        list.on_exit = Some(emit(&events, || ListEvent::Exit));
        list.on_toggle_scope = Some(emit(&events, || ListEvent::ToggleScope));
        list.on_toggle_sort = Some(emit(&events, || ListEvent::ToggleSort));
        list.on_toggle_name_filter = Some(emit(&events, || ListEvent::ToggleNameFilter));
        list.on_toggle_path = Some(emit_with(&events, ListEvent::TogglePath));
        list.on_delete_confirmation_change = Some(emit_with(&events, ListEvent::DeleteConfirmationChange));
        list.on_delete_session = Some(emit_with(&events, ListEvent::DeleteSession));
        list.on_rename_session = Some(emit_with(&events, ListEvent::RenameSession));
        list.on_error = Some(emit_with(&events, ListEvent::Error));

        let rename_submitted: Rc<RefCell<Option<String>>> = Rc::default();
        let mut rename_input = Input::default();
        let submitted = rename_submitted.clone();
        rename_input.set_on_submit(move |value| *submitted.borrow_mut() = Some(value.to_string()));

        let selector = Rc::new_cyclic(|this| {
            RefCell::new(Self {
                this: this.clone(),
                container: Container::new(),
                header: component(header),
                session_list: component(list),
                events,
                scope,
                sort_mode,
                name_filter,
                current_sessions: None,
                all_sessions: None,
                current_sessions_loader: Box::new(current_sessions_loader),
                all_sessions_loader: Box::new(all_sessions_loader),
                current_load: None,
                all_load: None,
                next_load_id: 0,
                progress: Rc::default(),
                request_render,
                rename_session: options.rename_session,
                mode: Mode::List,
                rename_input: component(rename_input),
                rename_submitted,
                rename_target_path: None,
                on_select: Box::new(on_select),
                on_cancel: Box::new(on_cancel),
                on_exit: Box::new(on_exit),
            })
        });
        {
            let mut this = selector.borrow_mut();
            let list: ComponentRef = this.session_list.clone();
            this.build_base_layout(list, true);
            this.load_scope(SessionScope::Current);
        }
        selector
    }

    /// The session list (upstream `getSessionList`).
    pub fn session_list(&self) -> Rc<RefCell<SessionList>> {
        self.session_list.clone()
    }

    pub fn header(&self) -> Rc<RefCell<SessionSelectorHeader>> {
        self.header.clone()
    }

    pub fn scope(&self) -> SessionScope {
        self.scope
    }

    pub fn is_renaming(&self) -> bool {
        self.mode == Mode::Rename
    }

    fn render_now(&self) {
        (self.request_render)();
    }

    fn build_base_layout(&mut self, content: ComponentRef, show_header: bool) {
        let accent_border = || component(DynamicBorder::with_color(|line: &str| fg(ThemeColor::Accent, line)));
        self.container.clear();
        self.container.add_child(spacer(1));
        self.container.add_child(accent_border());
        self.container.add_child(spacer(1));
        if show_header {
            self.container.add_child(self.header.clone());
            self.container.add_child(spacer(1));
        }
        self.container.add_child(content);
        self.container.add_child(spacer(1));
        self.container.add_child(accent_border());
    }

    fn enter_rename_mode(&mut self, path: PathBuf, current_name: Option<&str>) {
        self.mode = Mode::Rename;
        self.rename_target_path = Some(path);
        {
            let mut input = self.rename_input.borrow_mut();
            input.set_value(current_name.unwrap_or_default());
            input.focused = true;
        }
        let mut panel = Container::new();
        panel.add_child(text(&bold("Rename Session"), 1, 0));
        panel.add_child(spacer(1));
        panel.add_child(self.rename_input.clone());
        panel.add_child(spacer(1));
        let hint = format!("{} to save · {} to cancel", key_text("tui.select.confirm"), key_text("tui.select.cancel"));
        panel.add_child(text(&fg(ThemeColor::Muted, &hint), 1, 0));
        self.build_base_layout(component(panel), false);
        self.render_now();
    }

    fn exit_rename_mode(&mut self) {
        self.mode = Mode::List;
        self.rename_target_path = None;
        let list: ComponentRef = self.session_list.clone();
        self.build_base_layout(list, true);
        self.render_now();
    }

    fn leave(&mut self) {
        self.header.borrow_mut().set_status_message(None, None);
        self.cancel_loads();
    }

    fn process_events(&mut self) {
        loop {
            let events = std::mem::take(&mut *self.events.borrow_mut());
            if events.is_empty() {
                return;
            }
            for event in events {
                self.apply_event(event);
            }
        }
    }

    fn apply_event(&mut self, event: ListEvent) {
        match event {
            ListEvent::Select(path) => {
                self.leave();
                (self.on_select)(path);
            }
            ListEvent::Cancel => {
                self.leave();
                (self.on_cancel)();
            }
            ListEvent::Exit => {
                self.leave();
                (self.on_exit)();
            }
            ListEvent::ToggleScope => self.toggle_scope(),
            ListEvent::ToggleSort => self.toggle_sort_mode(),
            ListEvent::ToggleNameFilter => self.toggle_name_filter(),
            ListEvent::TogglePath(show_path) => {
                self.header.borrow_mut().set_show_path(show_path);
                self.render_now();
            }
            ListEvent::DeleteConfirmationChange(path) => {
                self.header.borrow_mut().set_confirming_delete_path(path);
                self.render_now();
            }
            ListEvent::Error(message) => {
                self.header.borrow_mut().set_status_message(Some(StatusMessage::error(&message)), Some(3000));
                self.render_now();
            }
            ListEvent::DeleteSession(path) => self.delete_session(path),
            ListEvent::RenameSession(path) => {
                if self.rename_session.is_none() || self.active_load(self.scope).is_some() {
                    return;
                }
                let sessions = self.scope_sessions(self.scope).as_deref().unwrap_or_default();
                let name =
                    sessions.iter().find(|session| session.path == path).and_then(|session| session.name.clone());
                self.enter_rename_mode(path, name.as_deref());
            }
        }
    }

    fn toggle_sort_mode(&mut self) {
        self.sort_mode = match self.sort_mode {
            SortMode::Threaded => SortMode::Recent,
            SortMode::Recent => SortMode::Relevance,
            SortMode::Relevance => SortMode::Threaded,
        };
        self.header.borrow_mut().set_sort_mode(self.sort_mode);
        self.session_list.borrow_mut().set_sort_mode(self.sort_mode);
        self.render_now();
    }

    fn toggle_name_filter(&mut self) {
        self.name_filter = if self.name_filter == NameFilter::All { NameFilter::Named } else { NameFilter::All };
        self.header.borrow_mut().set_name_filter(self.name_filter);
        self.session_list.borrow_mut().set_name_filter(self.name_filter);
        self.render_now();
    }

    fn toggle_scope(&mut self) {
        self.scope = if self.scope == SessionScope::Current { SessionScope::All } else { SessionScope::Current };
        let sessions = self.scope_sessions(self.scope).clone();
        let loading = self.active_load(self.scope).is_some();
        {
            let mut header = self.header.borrow_mut();
            header.set_scope(self.scope);
            header.set_loading(loading);
        }
        let show_cwd = self.scope == SessionScope::All;
        self.session_list.borrow_mut().set_sessions(sessions.clone().unwrap_or_default(), show_cwd);
        self.render_now();
        if sessions.is_none() && !loading {
            self.load_scope(self.scope);
        }
    }
}

impl Component for SessionSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        if self.mode == Mode::Rename {
            if get_keybindings().matches(data, "tui.select.cancel") {
                self.exit_rename_mode();
                return;
            }
            self.rename_input.borrow_mut().handle_input(data);
            let submitted = self.rename_submitted.borrow_mut().take();
            if let Some(value) = submitted {
                self.confirm_rename(&value);
            }
            return;
        }
        self.session_list.borrow_mut().handle_input(data);
        self.process_events();
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn captures_viewport_input(&self) -> bool {
        self.mode != Mode::Rename
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.session_list.borrow_mut().set_focused(focused);
        self.rename_input.borrow_mut().focused = focused;
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}

impl Drop for SessionSelector {
    fn drop(&mut self) {
        for load in [self.current_load.take(), self.all_load.take()].into_iter().flatten() {
            load.cancel();
        }
    }
}
