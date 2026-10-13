//! Hosting of selectors and dialogs (upstream `InteractiveMode.showSelector`): the host swaps a
//! selector component in place of the editor and puts the editor back when `done` runs.
//! Callbacks run on the UI thread, so they may hold `Rc` state and call back into the host.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use pi_tui::{ComponentRef, Container, Tui};

/// Restores the editor in place of the selector that received it. Calling it again, or after
/// another selector replaced this one, does nothing.
pub type SelectorDone = Rc<dyn Fn()>;

/// What a selector factory returns.
pub struct SelectorSpec {
    /// Shown in place of the editor.
    pub component: ComponentRef,
    /// Receives focus; usually `component` itself.
    pub focus: ComponentRef,
    /// Runs once when the selector closes or another selector replaces it.
    pub dispose: Option<Box<dyn FnOnce()>>,
}

impl SelectorSpec {
    /// A selector that is its own focus target and needs no cleanup.
    pub fn new(component: ComponentRef) -> Self {
        Self { focus: component.clone(), component, dispose: None }
    }

    pub fn with_focus(mut self, focus: ComponentRef) -> Self {
        self.focus = focus;
        self
    }

    pub fn with_dispose(mut self, dispose: impl FnOnce() + 'static) -> Self {
        self.dispose = Some(Box::new(dispose));
        self
    }
}

/// Entry points dialogs and slash commands use to show UI in the editor slot.
pub trait SelectorHost {
    /// Replace the editor with the selector `create` builds; `create` receives its `done`.
    fn show_selector(&self, create: Box<dyn FnOnce(SelectorDone) -> SelectorSpec>);
    /// Show a dim status line in the chat (upstream `showStatus`).
    fn show_status(&self, message: &str);
    /// Show an error line in the chat (upstream `showError`).
    fn show_error(&self, message: &str);
    /// Show a warning line in the chat (upstream `showWarning`).
    fn show_warning(&self, message: &str);
    fn tui(&self) -> Tui;
}

struct Active {
    token: u64,
    dispose: Option<Box<dyn FnOnce()>>,
}

/// The editor slot: a container that holds the editor or the active selector.
pub struct SelectorSlot {
    tui: Tui,
    container: Rc<RefCell<Container>>,
    editor: RefCell<ComponentRef>,
    active: RefCell<Option<Active>>,
    next_token: Cell<u64>,
}

impl SelectorSlot {
    pub fn new(tui: Tui, container: Rc<RefCell<Container>>, editor: ComponentRef) -> Rc<Self> {
        Rc::new(Self {
            tui,
            container,
            editor: RefCell::new(editor),
            active: RefCell::new(None),
            next_token: Cell::new(1),
        })
    }

    pub fn container(&self) -> Rc<RefCell<Container>> {
        self.container.clone()
    }

    pub fn editor(&self) -> ComponentRef {
        self.editor.borrow().clone()
    }

    /// Swap the editor component (for example an extension's custom editor).
    pub fn set_editor(&self, editor: ComponentRef) {
        let showing_editor = self.active.borrow().is_none();
        *self.editor.borrow_mut() = editor.clone();
        if showing_editor {
            let mut container = self.container.borrow_mut();
            container.clear();
            container.add_child(editor);
        }
    }

    /// Whether a selector currently replaces the editor.
    pub fn is_active(&self) -> bool {
        self.active.borrow().is_some()
    }

    /// Close the active selector without running its callbacks (upstream `disposeActiveSelector`).
    pub fn dispose_active(&self) {
        let active = self.active.borrow_mut().take();
        if let Some(dispose) = active.and_then(|active| active.dispose) {
            dispose();
        }
    }

    /// Put the editor back and focus it.
    pub fn restore_editor(&self) {
        self.dispose_active();
        let editor = self.editor();
        {
            let mut container = self.container.borrow_mut();
            container.clear();
            container.add_child(editor.clone());
        }
        self.tui.set_focus(Some(editor));
        self.tui.request_render(false);
    }

    pub fn show(self: &Rc<Self>, create: Box<dyn FnOnce(SelectorDone) -> SelectorSpec>) {
        let token = self.next_token.get();
        self.next_token.set(token + 1);
        let weak = Rc::downgrade(self);
        let done: SelectorDone = Rc::new(move || {
            let Some(slot) = weak.upgrade() else { return };
            let is_current = slot.active.borrow().as_ref().is_some_and(|active| active.token == token);
            if is_current {
                slot.restore_editor();
            }
        });
        let spec = create(done);
        self.dispose_active();
        *self.active.borrow_mut() = Some(Active { token, dispose: spec.dispose });
        {
            let mut container = self.container.borrow_mut();
            container.clear();
            container.add_child(spec.component);
        }
        self.tui.set_focus(Some(spec.focus));
        self.tui.request_render(false);
    }
}
