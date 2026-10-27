//! [`ModelSelector`]: the `/model` picker with search, all/scoped tabs, save-as-default, and a
//! background catalog refresh (upstream `model-selector.ts`).

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use pi_ai::Model;
use pi_tui::components::{Input, Text};
use pi_tui::{Component, Container, Tui, component, get_keybindings};
use tokio_util::sync::CancellationToken;

mod source;

pub use source::{
    DefaultModelReference, ModelRefreshFn, ModelSelectorSource, ModelsRefreshResult, distinct_model_name,
    model_search_text, model_selector_search_text,
};

use super::common::{border, fg, fuzzy_filter_by, locale_compare, spacer, text, visible_window};
use super::{key_display_text, key_hint};
use crate::model_resolver::ScopedModel;
use crate::theme::ThemeColor;

const MAX_VISIBLE: usize = 10;
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    All,
    Scoped,
}

/// Called with a chosen model.
pub type ModelCallback = Box<dyn FnMut(&Model)>;

/// Optional parts of a [`ModelSelector`].
#[derive(Default)]
pub struct ModelSelectorOptions {
    pub initial_search_input: Option<String>,
    /// Enables `app.models.save` and its hint.
    pub on_select_as_default: Option<ModelCallback>,
    pub default_model: Option<DefaultModelReference>,
    /// Refreshes catalogs in the background; without it the selector shows the snapshot only.
    pub refresh: Option<ModelRefreshFn>,
}

fn models_equal(a: Option<&Model>, b: &Model) -> bool {
    a.is_some_and(|a| a.provider == b.provider && a.id == b.id)
}

pub struct ModelSelector {
    container: Container,
    search_input: Rc<RefCell<Input>>,
    submitted: Rc<Cell<bool>>,
    list: Rc<RefCell<Container>>,
    scope_text: Option<Rc<RefCell<Text>>>,
    scope_hint_text: Option<Rc<RefCell<Text>>>,
    source: Rc<dyn ModelSelectorSource>,
    tui: Tui,
    all_models: Vec<Model>,
    scoped: Vec<ScopedModel>,
    scoped_items: Vec<Model>,
    active: Vec<Model>,
    filtered: Vec<Model>,
    selected_index: usize,
    current_model: Option<Model>,
    default_model: Option<DefaultModelReference>,
    scope: Scope,
    error_message: Option<String>,
    refresh_status: String,
    refresh_success: bool,
    on_select: ModelCallback,
    on_cancel: Box<dyn FnMut()>,
    on_select_as_default: Option<ModelCallback>,
    refresh_cancel: CancellationToken,
    closed: Rc<Cell<bool>>,
}

impl ModelSelector {
    /// Shows the snapshot at once and refreshes catalogs in the background when
    /// `options.refresh` is set (on a `spawn_local` task, so it needs a `LocalSet`).
    pub fn new(
        tui: &Tui,
        current_model: Option<Model>,
        source: Rc<dyn ModelSelectorSource>,
        scoped_models: Vec<ScopedModel>,
        on_select: impl FnMut(&Model) + 'static,
        on_cancel: impl FnMut() + 'static,
        options: ModelSelectorOptions,
    ) -> Rc<RefCell<Self>> {
        let scope = if scoped_models.is_empty() { Scope::All } else { Scope::Scoped };
        let mut container = Container::new();
        container.add_child(border());
        container.add_child(spacer(1));
        let (mut scope_text, mut scope_hint_text) = (None, None);
        if scoped_models.is_empty() {
            let hint = "Only showing models from configured providers. Use /login to add providers.";
            container.add_child(text(&fg(ThemeColor::Warning, hint), 0, 0));
        } else {
            let scope_line = component(Text::new(&scope_line(scope), 0, 0));
            container.add_child(scope_line.clone());
            let hint = component(Text::new(&scope_hint(), 0, 0));
            container.add_child(hint.clone());
            (scope_text, scope_hint_text) = (Some(scope_line), Some(hint));
        }
        container.add_child(spacer(1));

        let submitted = Rc::new(Cell::new(false));
        let mut input = Input::default();
        let initial = options.initial_search_input.clone().filter(|initial| !initial.is_empty());
        if let Some(initial) = &initial {
            input.set_value(initial);
        }
        let flag = submitted.clone();
        input.set_on_submit(move |_| flag.set(true));
        let search_input = component(input);
        container.add_child(search_input.clone());
        container.add_child(spacer(1));
        let list = component(Container::new());
        container.add_child(list.clone());
        container.add_child(spacer(1));
        if options.on_select_as_default.is_some() {
            let hint = format!(
                "  {} to select · {} to set as default · {} to cancel",
                key_display_text("tui.select.confirm"),
                key_display_text("app.models.save"),
                key_display_text("tui.select.cancel"),
            );
            container.add_child(text(&fg(ThemeColor::Dim, &hint), 0, 0));
        }
        container.add_child(border());

        let refresh_status = if options.refresh.is_some() { "Refreshing model catalogs…" } else { "" };
        let mut selector = Self {
            container,
            search_input,
            submitted,
            list,
            scope_text,
            scope_hint_text,
            source,
            tui: tui.clone(),
            all_models: Vec::new(),
            scoped: scoped_models,
            scoped_items: Vec::new(),
            active: Vec::new(),
            filtered: Vec::new(),
            selected_index: 0,
            current_model,
            default_model: options.default_model,
            scope,
            error_message: None,
            refresh_status: refresh_status.to_string(),
            refresh_success: false,
            on_select: Box::new(on_select),
            on_cancel: Box::new(on_cancel),
            on_select_as_default: options.on_select_as_default,
            refresh_cancel: CancellationToken::new(),
            closed: Rc::new(Cell::new(false)),
        };
        selector.load_models_from_snapshot();
        match &initial {
            Some(initial) => selector.filter_models(initial),
            None => selector.update_list(),
        }
        tui.request_render(false);
        let refresh = options.refresh;
        let selector = Rc::new(RefCell::new(selector));
        if let Some(refresh) = refresh {
            let token = selector.borrow().refresh_cancel.clone();
            tokio::task::spawn_local(run_refresh(Rc::downgrade(&selector), refresh, token));
        }
        selector
    }

    fn is_default_model(&self, model: &Model) -> bool {
        self.default_model.as_ref().is_some_and(|default| default.provider == model.provider && default.id == model.id)
    }

    fn sort_models(&self, mut models: Vec<Model>) -> Vec<Model> {
        let current = self.current_model.as_ref();
        models.sort_by(|a, b| {
            let (a_current, b_current) = (models_equal(current, a), models_equal(current, b));
            let (a_default, b_default) = (self.is_default_model(a), self.is_default_model(b));
            b_current
                .cmp(&a_current)
                .then(b_default.cmp(&a_default))
                .then_with(|| locale_compare(&a.provider, &b.provider))
        });
        models
    }

    fn load_models_from_snapshot(&mut self) {
        self.all_models = self.sort_models(self.source.available_models());
        for scoped in &mut self.scoped {
            if let Some(refreshed) = self.source.find_model(&scoped.model.provider, &scoped.model.id) {
                scoped.model = refreshed;
            }
        }
        self.scoped_items = self.scoped.iter().map(|scoped| scoped.model.clone()).collect();
        self.active = self.active_models();
        self.filtered = self.active.clone();
        let current = self.current_model.as_ref();
        self.selected_index = match self.filtered.iter().position(|model| models_equal(current, model)) {
            Some(index) => index,
            None => self.selected_index.min(self.filtered.len().saturating_sub(1)),
        };
    }

    fn active_models(&self) -> Vec<Model> {
        match self.scope {
            Scope::Scoped => self.scoped_items.clone(),
            Scope::All => self.all_models.clone(),
        }
    }

    fn set_scope(&mut self, scope: Scope) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.active = self.active_models();
        let current = self.current_model.as_ref();
        self.selected_index = self.active.iter().position(|model| models_equal(current, model)).unwrap_or(0);
        let query = self.search_input.borrow().get_value().to_string();
        self.filter_models(&query);
        if let Some(scope_text) = &self.scope_text {
            scope_text.borrow_mut().set_text(&scope_line(self.scope));
        }
    }

    fn filter_models(&mut self, query: &str) {
        if query.is_empty() {
            self.filtered = self.active.clone();
        } else {
            let filtered = fuzzy_filter_by(&self.active, query, |model| {
                let default = if self.is_default_model(model) { " default" } else { "" };
                format!("{}{default}", model_selector_search_text(model))
            });
            let normalized = query.trim().to_lowercase();
            if !normalized.is_empty() && "default".starts_with(&normalized) {
                let defaults: Vec<Model> =
                    self.active.iter().filter(|model| self.is_default_model(model)).cloned().collect();
                let rest = filtered.into_iter().filter(|model| !self.is_default_model(model));
                self.filtered = defaults.into_iter().chain(rest).collect();
            } else {
                self.filtered = filtered;
            }
        }
        self.selected_index =
            if query.is_empty() { self.selected_index.min(self.filtered.len().saturating_sub(1)) } else { 0 };
        self.update_list();
    }

    fn update_list(&mut self) {
        let mut list = self.list.borrow_mut();
        list.clear();
        let (start, end) = visible_window(self.selected_index, self.filtered.len(), MAX_VISIBLE);
        let current = self.current_model.as_ref();
        for (index, model) in self.filtered.iter().enumerate().take(end).skip(start) {
            let selected = index == self.selected_index;
            let cursor = if selected { fg(ThemeColor::Accent, "→ ") } else { "  ".into() };
            let marker = if models_equal(current, model) { fg(ThemeColor::Accent, "✓ ") } else { "  ".into() };
            let id = if selected { fg(ThemeColor::Accent, &model.id) } else { model.id.clone() };
            let name = distinct_model_name(&model.id, &model.name)
                .map_or_else(String::new, |name| fg(ThemeColor::Muted, &format!(" ({name})")));
            let provider = fg(ThemeColor::Muted, &format!("[{}]", model.provider));
            let badge = if self.is_default_model(model) { fg(ThemeColor::Muted, " · default") } else { String::new() };
            list.add_child(text(&format!("{cursor}{marker}{id}{name} {provider}{badge}"), 0, 0));
        }
        if start > 0 || end < self.filtered.len() {
            let info = format!("  ({}/{})", self.selected_index + 1, self.filtered.len());
            list.add_child(text(&fg(ThemeColor::Muted, &info), 0, 0));
        }
        if let Some(error) = &self.error_message {
            for line in error.split('\n') {
                list.add_child(text(&fg(ThemeColor::Error, line), 0, 0));
            }
        } else if let Some(selected) = self.filtered.get(self.selected_index) {
            list.add_child(spacer(1));
            list.add_child(text(&fg(ThemeColor::Muted, &format!("  Model Name: {}", selected.name)), 0, 0));
        } else {
            list.add_child(text(&fg(ThemeColor::Muted, "  No matching models"), 0, 0));
        }
        if !self.refresh_status.is_empty() {
            list.add_child(spacer(1));
            let color = if self.refresh_success { ThemeColor::Success } else { ThemeColor::Muted };
            list.add_child(text(&fg(color, &format!("  {}", self.refresh_status)), 0, 0));
        }
    }

    fn apply_refresh(&mut self, outcome: Option<Result<ModelsRefreshResult, String>>) {
        self.refresh_status.clear();
        let result = match outcome {
            None => {
                self.error_message = Some("Model refresh timed out; showing cached models.".into());
                self.update_list();
                return;
            }
            Some(Err(error)) => {
                self.error_message = Some(format!("Could not refresh model catalogs: {error}"));
                self.update_list();
                return;
            }
            Some(Ok(result)) => result,
        };
        self.error_message = match result.errors.as_slice() {
            [] => {
                let error = self.source.catalog_error();
                if error.is_none() {
                    self.refresh_status = "Model catalogs refreshed.".into();
                    self.refresh_success = true;
                }
                error
            }
            [provider] => Some(format!("Could not refresh {provider}; showing cached models.")),
            errors => Some(format!(
                "Could not refresh {} model catalogs ({}); showing cached models.",
                errors.len(),
                errors.join(", ")
            )),
        };
        self.load_models_from_snapshot();
        let query = self.search_input.borrow().get_value().to_string();
        self.filter_models(&query);
    }

    /// Stops the background refresh.
    pub fn dispose(&mut self) {
        self.closed.set(true);
        self.refresh_cancel.cancel();
    }

    /// Like [`Self::dispose`]; usable from inside a callback.
    pub fn disposer(&self) -> impl Fn() + 'static {
        let (closed, token) = (self.closed.clone(), self.refresh_cancel.clone());
        move || {
            closed.set(true);
            token.cancel();
        }
    }

    pub fn search_input(&self) -> Rc<RefCell<Input>> {
        self.search_input.clone()
    }

    fn select(&mut self) {
        if let Some(model) = self.filtered.get(self.selected_index).cloned() {
            self.dispose();
            (self.on_select)(&model);
        }
    }
}

fn scope_line(scope: Scope) -> String {
    let all = fg(if scope == Scope::All { ThemeColor::Accent } else { ThemeColor::Muted }, "all");
    let scoped = fg(if scope == Scope::Scoped { ThemeColor::Accent } else { ThemeColor::Muted }, "scoped");
    format!("{}{all}{}{scoped}", fg(ThemeColor::Muted, "Scope: "), fg(ThemeColor::Muted, " | "))
}

fn scope_hint() -> String {
    key_hint("tui.input.tab", "scope") + &fg(ThemeColor::Muted, " (all/scoped)")
}

async fn run_refresh(selector: Weak<RefCell<ModelSelector>>, refresh: ModelRefreshFn, token: CancellationToken) {
    let future = refresh(token.clone());
    let outcome = tokio::select! {
        result = future => Some(result),
        () = tokio::time::sleep(REFRESH_TIMEOUT) => {
            token.cancel();
            None
        }
    };
    let Some(selector) = selector.upgrade() else { return };
    let mut selector = selector.borrow_mut();
    if selector.closed.get() {
        return;
    }
    selector.apply_refresh(outcome);
    selector.tui.request_render(false);
}

impl Component for ModelSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        let len = self.filtered.len();
        if kb.matches(data, "tui.input.tab") {
            if !self.scoped_items.is_empty() {
                self.set_scope(if self.scope == Scope::All { Scope::Scoped } else { Scope::All });
                if let Some(hint) = &self.scope_hint_text {
                    hint.borrow_mut().set_text(&scope_hint());
                }
            }
        } else if kb.matches(data, "tui.select.up") {
            if len == 0 {
                return;
            }
            self.selected_index = if self.selected_index == 0 { len - 1 } else { self.selected_index - 1 };
            self.update_list();
        } else if kb.matches(data, "tui.select.down") {
            if len == 0 {
                return;
            }
            self.selected_index = if self.selected_index + 1 == len { 0 } else { self.selected_index + 1 };
            self.update_list();
        } else if kb.matches(data, "tui.select.confirm") {
            self.select();
        } else if kb.matches(data, "tui.select.cancel") {
            self.dispose();
            (self.on_cancel)();
        } else if kb.matches(data, "app.models.save") && self.on_select_as_default.is_some() {
            if let Some(model) = self.filtered.get(self.selected_index).cloned() {
                self.dispose();
                if let Some(callback) = self.on_select_as_default.as_mut() {
                    callback(&model);
                }
            }
        } else {
            self.search_input.borrow_mut().handle_input(data);
            if self.submitted.replace(false) {
                self.select();
            }
            let query = self.search_input.borrow().get_value().to_string();
            self.filter_models(&query);
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.search_input.borrow_mut().focused = focused;
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
