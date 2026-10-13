//! [`SelectSubmenu`] and [`SteppedSubmenu`]: titled select lists that settings submenus use
//! (upstream `settings-submenu.ts`).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use pi_tui::components::{Input, SelectItem, SelectList, SelectListLayoutOptions};
use pi_tui::{Component, ComponentRef, Container, get_keybindings};

use super::super::common::{Shared, bold, fg, fuzzy_filter_by, shared, spacer, text};
use super::config::OnText;
use crate::theme::{ThemeColor, get_select_list_theme};

/// Column widths of submenu lists unless a step overrides them.
pub fn submenu_layout() -> SelectListLayoutOptions {
    SelectListLayoutOptions {
        min_primary_column_width: Some(12),
        max_primary_column_width: Some(32),
        truncate_primary: None,
    }
}

/// Options of [`SelectSubmenu::new`].
#[derive(Clone, Default)]
pub struct SelectSubmenuOptions {
    /// Typing filters the list with fuzzy matching.
    pub searchable: bool,
    /// Overrides the select list column widths.
    pub layout: Option<SelectListLayoutOptions>,
}

type TextCallback = Shared<dyn FnMut(&str)>;

#[derive(Clone)]
struct ListCallbacks {
    on_select: Shared<dyn FnMut(&str)>,
    on_cancel: Shared<dyn FnMut()>,
    on_selection_change: Option<TextCallback>,
}

/// Single-step submenu with a title, a description, and a select list. With `searchable`,
/// typing filters the list.
pub struct SelectSubmenu {
    container: Container,
    select_list: Rc<RefCell<SelectList>>,
    list_index: usize,
    all_options: Vec<SelectItem>,
    layout: SelectListLayoutOptions,
    search_input: Option<Rc<RefCell<Input>>>,
    submitted: Rc<Cell<bool>>,
    callbacks: ListCallbacks,
}

impl SelectSubmenu {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        title: &str,
        description: &str,
        options: Vec<SelectItem>,
        current_value: &str,
        on_select: impl FnMut(&str) + 'static,
        on_cancel: impl FnMut() + 'static,
        on_selection_change: Option<OnText>,
        submenu_options: SelectSubmenuOptions,
    ) -> Self {
        let layout = submenu_options.layout.clone().unwrap_or_else(submenu_layout);
        let on_select: Shared<dyn FnMut(&str)> = shared(Box::new(on_select));
        let on_cancel: Shared<dyn FnMut()> = shared(Box::new(on_cancel));
        let callbacks = ListCallbacks { on_select, on_cancel, on_selection_change: on_selection_change.map(shared) };

        let mut container = Container::new();
        container.add_child(text(&bold(&fg(ThemeColor::Accent, title)), 0, 0));
        if !description.is_empty() {
            container.add_child(spacer(1));
            container.add_child(text(&fg(ThemeColor::Muted, description), 0, 0));
        }
        let submitted = Rc::new(Cell::new(false));
        let search_input = submenu_options.searchable.then(|| {
            container.add_child(spacer(1));
            let mut input = Input::default();
            let flag = submitted.clone();
            input.set_on_submit(move |_| flag.set(true));
            let input = Rc::new(RefCell::new(input));
            container.add_child(input.clone());
            input
        });
        container.add_child(spacer(1));
        let select_list = build_list(&options, current_value, &layout, &callbacks);
        let list_index = container.children.len();
        container.add_child(select_list.clone());
        container.add_child(spacer(1));
        let hint = if submenu_options.searchable {
            "  Type to filter · Enter to select · Esc to go back"
        } else {
            "  Enter to select · Esc to go back"
        };
        container.add_child(text(&fg(ThemeColor::Dim, hint), 0, 0));

        Self { container, select_list, list_index, all_options: options, layout, search_input, submitted, callbacks }
    }

    pub fn select_list(&self) -> Rc<RefCell<SelectList>> {
        self.select_list.clone()
    }

    fn apply_filter(&mut self, query: &str) {
        let filtered = if query.is_empty() {
            self.all_options.clone()
        } else {
            fuzzy_filter_by(&self.all_options, query, |item: &SelectItem| {
                format!("{} {}", item.label, item.description.as_deref().unwrap_or(""))
            })
        };
        let list = build_list(&filtered, "", &self.layout, &self.callbacks);
        let erased: ComponentRef = list.clone();
        self.container.children[self.list_index] = erased;
        self.select_list = list;
    }
}

fn build_list(
    options: &[SelectItem],
    preselect: &str,
    layout: &SelectListLayoutOptions,
    callbacks: &ListCallbacks,
) -> Rc<RefCell<SelectList>> {
    let mut list = SelectList::new(options.to_vec(), options.len().min(10), get_select_list_theme(), layout.clone());
    if let Some(index) = options.iter().position(|option| option.value == preselect) {
        list.set_selected_index(index);
    }
    let on_select = callbacks.on_select.clone();
    list.on_select = Some(Box::new(move |item: &SelectItem| (on_select.borrow_mut())(&item.value)));
    let on_cancel = callbacks.on_cancel.clone();
    list.on_cancel = Some(Box::new(move || (on_cancel.borrow_mut())()));
    if let Some(on_change) = callbacks.on_selection_change.clone() {
        list.on_selection_change = Some(Box::new(move |item: &SelectItem| (on_change.borrow_mut())(&item.value)));
    }
    Rc::new(RefCell::new(list))
}

impl Component for SelectSubmenu {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        let Some(input) = self.search_input.clone() else {
            self.select_list.borrow_mut().handle_input(data);
            return;
        };
        let kb = get_keybindings();
        let is_nav = ["tui.select.up", "tui.select.down", "tui.select.confirm", "tui.select.cancel"]
            .iter()
            .any(|id| kb.matches(data, id));
        if is_nav {
            self.select_list.borrow_mut().handle_input(data);
            return;
        }
        input.borrow_mut().handle_input(data);
        if self.submitted.replace(false) {
            self.select_list.borrow_mut().handle_input("\r");
        }
        let query = input.borrow().get_value().to_string();
        self.apply_filter(&query);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}

/// Selections of a [`SteppedSubmenu`], keyed by step key.
pub type StepContext = HashMap<String, String>;

/// A step title or description: fixed, or computed from prior selections.
pub enum StepText {
    Fixed(String),
    Dynamic(Box<dyn Fn(&StepContext) -> String>),
}

impl StepText {
    fn resolve(&self, context: &StepContext) -> String {
        match self {
            StepText::Fixed(value) => value.clone(),
            StepText::Dynamic(compute) => compute(context),
        }
    }
}

impl From<&str> for StepText {
    fn from(value: &str) -> Self {
        StepText::Fixed(value.to_string())
    }
}

/// Builds the options of a step from prior selections.
pub type StepOptionsFn = Box<dyn Fn(&StepContext) -> Vec<SelectItem>>;
/// The value to preselect in a step.
pub type StepPreselectFn = Box<dyn Fn(&StepContext) -> Option<String>>;

/// One step of a [`SteppedSubmenu`]; its selection is stored in the context under `key`.
pub struct SteppedSubmenuStep {
    pub key: String,
    pub title: StepText,
    pub description: StepText,
    /// Builds the options each time the step is shown.
    pub options: StepOptionsFn,
    /// The value to preselect when entering the step.
    pub preselect: Option<StepPreselectFn>,
    pub searchable: bool,
    pub layout: Option<SelectListLayoutOptions>,
}

/// Options of [`SteppedSubmenu::new`].
#[derive(Clone, Default)]
pub struct SteppedSubmenuOptions {
    /// Starts at this step; `initial_context` supplies the skipped selections.
    pub start_at_step: usize,
    pub initial_context: StepContext,
    /// After the last step, report the result and return to step 0 instead of closing.
    pub loop_back: bool,
}

enum StepEvent {
    Selected(String),
    Cancelled,
}

/// N-step selector built from [`SelectSubmenu`]s. Esc goes back one step and cancels at step 0.
pub struct SteppedSubmenu {
    steps: Vec<SteppedSubmenuStep>,
    on_complete: Box<dyn FnMut(&StepContext)>,
    on_cancel: Box<dyn FnMut()>,
    options: SteppedSubmenuOptions,
    active: SelectSubmenu,
    active_step: usize,
    context: StepContext,
    events: Rc<RefCell<Vec<StepEvent>>>,
}

impl SteppedSubmenu {
    pub fn new(
        steps: Vec<SteppedSubmenuStep>,
        on_complete: impl FnMut(&StepContext) + 'static,
        on_cancel: impl FnMut() + 'static,
        options: SteppedSubmenuOptions,
    ) -> Self {
        let context = options.initial_context.clone();
        let events = Rc::new(RefCell::new(Vec::new()));
        let start = options.start_at_step;
        let active = build_step(&steps, start, &context, &events);
        Self {
            steps,
            on_complete: Box::new(on_complete),
            on_cancel: Box::new(on_cancel),
            options,
            active,
            active_step: start,
            context,
            events,
        }
    }

    /// The submenu of the current step.
    pub fn active(&self) -> &SelectSubmenu {
        &self.active
    }

    fn show_step(&mut self, index: usize) {
        self.events.borrow_mut().clear();
        self.active = build_step(&self.steps, index, &self.context, &self.events);
        self.active_step = index;
    }

    fn process_events(&mut self) {
        loop {
            let event = {
                let mut events = self.events.borrow_mut();
                if events.is_empty() {
                    return;
                }
                events.remove(0)
            };
            let index = self.active_step;
            let key = self.steps[index].key.clone();
            match event {
                StepEvent::Selected(value) => {
                    self.context.insert(key, value);
                    if index + 1 < self.steps.len() {
                        self.show_step(index + 1);
                    } else {
                        let result = self.context.clone();
                        (self.on_complete)(&result);
                        if self.options.loop_back {
                            self.context.clear();
                            self.show_step(0);
                        } else {
                            (self.on_cancel)();
                        }
                    }
                }
                StepEvent::Cancelled if index > 0 => {
                    self.context.remove(&key);
                    self.show_step(index - 1);
                }
                StepEvent::Cancelled => (self.on_cancel)(),
            }
        }
    }
}

fn build_step(
    steps: &[SteppedSubmenuStep],
    index: usize,
    context: &StepContext,
    events: &Rc<RefCell<Vec<StepEvent>>>,
) -> SelectSubmenu {
    let step = &steps[index];
    let total = steps.len();
    let step_label = if total > 1 { format!("Step {}/{total} · ", index + 1) } else { String::new() };
    let title = step.title.resolve(context);
    let description = step.description.resolve(context);
    let items = (step.options)(context);
    let preselect = step.preselect.as_ref().and_then(|preselect| preselect(context)).unwrap_or_default();
    let (selected, cancelled) = (events.clone(), events.clone());
    SelectSubmenu::new(
        &title,
        &format!("{step_label}{description}"),
        items,
        &preselect,
        move |value: &str| selected.borrow_mut().push(StepEvent::Selected(value.to_string())),
        move || cancelled.borrow_mut().push(StepEvent::Cancelled),
        None,
        SelectSubmenuOptions { searchable: step.searchable, layout: step.layout.clone() },
    )
}

impl Component for SteppedSubmenu {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.active.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.active.handle_input(data);
        self.process_events();
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn invalidate(&mut self) {
        self.active.invalidate();
    }
}
