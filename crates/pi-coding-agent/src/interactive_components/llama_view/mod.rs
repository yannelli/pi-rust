//! The `/llama` view (upstream `extensions/llama/ui.ts`): the router's models, selects, the
//! Hugging Face search, status, and download or load progress. [`LlamaView::bridge`] turns it
//! into the [`crate::llama::LlamaUi`] that [`crate::llama::LlamaManager`] drives.

mod bridge;
mod search;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pi_tui::components::select_list::SelectListTheme;
use pi_tui::components::{SelectItem, SelectList, SelectListLayoutOptions};
use pi_tui::{Component, ComponentRef, Container, Tui, component, get_keybindings};

pub use bridge::LlamaViewUi;
pub use search::{HuggingFaceSearch, HuggingFaceSearchFn, SearchCache, hugging_face_search_fn};

use super::common::{clamp_lines, fg, frame, spacer, text};
use super::key_hint;
use crate::llama::view::{
    DOWNLOAD_DESCRIPTION, DOWNLOAD_LABEL, ProgressState, model_description, progress_bar, sort_models_for_display,
};
use crate::llama::{LlamaManagerAction, LlamaModelInfo};
use crate::theme::ThemeColor;

const DOWNLOAD_VALUE: &str = "\0download";
const TITLE: &str = "llama.cpp models";

type Resolve<T> = Rc<RefCell<Option<Box<dyn FnOnce(T)>>>>;

fn resolver<T: 'static>(callback: impl FnOnce(T) + 'static) -> Resolve<T> {
    Rc::new(RefCell::new(Some(Box::new(callback))))
}

fn resolve<T>(slot: &Resolve<T>, value: T) {
    let callback = slot.borrow_mut().take();
    if let Some(callback) = callback {
        callback(value);
    }
}

fn select_theme() -> SelectListTheme {
    let style = |color: ThemeColor| -> pi_tui::components::StyleFn { Rc::new(move |text: &str| fg(color, text)) };
    SelectListTheme {
        selected_prefix: style(ThemeColor::Accent),
        selected_text: style(ThemeColor::Accent),
        description: style(ThemeColor::Muted),
        scroll_info: style(ThemeColor::Dim),
        no_match: style(ThemeColor::Warning),
    }
}

/// Upstream `LlamaView`. Each screen replaces the one shown; callbacks fire at most once.
pub struct LlamaView {
    tui: Tui,
    content: Container,
    input_handler: Option<ComponentRef>,
    input_target: Option<ComponentRef>,
    progress_waiters: Vec<Box<dyn FnOnce()>>,
    showing_progress: bool,
    focused: bool,
    search_cache: SearchCache,
}

impl LlamaView {
    /// Starts on a loading frame.
    pub fn new(tui: &Tui) -> Self {
        let loading = text(&fg(ThemeColor::Muted, "Loading…"), 1, 1);
        Self {
            tui: tui.clone(),
            content: frame(TITLE, vec![loading], None),
            input_handler: None,
            input_target: None,
            progress_waiters: Vec::new(),
            showing_progress: false,
            focused: false,
            search_cache: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    fn set_content(&mut self, content: Container, handler: Option<ComponentRef>, target: Option<ComponentRef>) {
        if let Some(target) = &self.input_target {
            target.borrow_mut().set_focused(false);
        }
        self.progress_waiters.clear();
        self.showing_progress = false;
        self.content = content;
        self.input_handler = handler;
        self.input_target = target;
        if let Some(target) = &self.input_target {
            target.borrow_mut().set_focused(self.focused);
        }
        self.tui.request_render(false);
    }

    /// The router's models (loaded first) and the download entry.
    pub fn show_models(
        &mut self,
        server_url: &str,
        models: &[LlamaModelInfo],
        on_action: impl FnOnce(LlamaManagerAction) + 'static,
    ) {
        let sorted = sort_models_for_display(models);
        let mut items: Vec<SelectItem> = sorted
            .iter()
            .map(|model| SelectItem::new(&model.id, &model.id).with_description(&model_description(model)))
            .collect();
        items.push(SelectItem::new(DOWNLOAD_VALUE, DOWNLOAD_LABEL).with_description(DOWNLOAD_DESCRIPTION));
        let layout = SelectListLayoutOptions {
            min_primary_column_width: Some(36),
            max_primary_column_width: Some(56),
            ..SelectListLayoutOptions::default()
        };
        let visible = items.len().min(12);
        let mut list = SelectList::new(items, visible, select_theme(), layout);
        let slot = resolver(on_action);
        let on_select = slot.clone();
        list.on_select = Some(Box::new(move |item: &SelectItem| {
            if item.value == DOWNLOAD_VALUE {
                resolve(&on_select, LlamaManagerAction::Download);
            } else if let Some(model) = sorted.iter().find(|model| model.id == item.value) {
                resolve(&on_select, LlamaManagerAction::Model(Box::new(model.clone())));
            }
        }));
        list.on_cancel = Some(Box::new(move || resolve(&slot, LlamaManagerAction::Close)));
        let list: ComponentRef = component(list);
        let footer = format!(
            "{} • {}",
            key_hint("tui.select.confirm", "load/unload/download"),
            key_hint("tui.select.cancel", "close")
        );
        let body = vec![text(&fg(ThemeColor::Dim, server_url), 1, 0), spacer(1), list.clone()];
        self.set_content(frame(TITLE, body, Some(&footer)), Some(list), None);
    }

    /// A list of `options` under `title`; `None` when cancelled.
    pub fn select(&mut self, title: &str, options: &[String], on_choice: impl FnOnce(Option<String>) + 'static) {
        let items: Vec<SelectItem> = options.iter().map(|option| SelectItem::new(option, option)).collect();
        let mut list = SelectList::new(items, options.len().min(12), select_theme(), Default::default());
        let slot = resolver(on_choice);
        let on_select = slot.clone();
        list.on_select = Some(Box::new(move |item: &SelectItem| resolve(&on_select, Some(item.value.clone()))));
        list.on_cancel = Some(Box::new(move || resolve(&slot, None)));
        let list: ComponentRef = component(list);
        let footer =
            format!("{} • {}", key_hint("tui.select.confirm", "select"), key_hint("tui.select.cancel", "cancel"));
        self.set_content(frame(title, vec![spacer(1), list.clone()], Some(&footer)), Some(list), None);
    }

    /// The Hugging Face search; `on_choice` gets `owner/repository[:quant]` or `None` (back).
    pub fn search_models(&mut self, search: HuggingFaceSearchFn, on_choice: impl FnOnce(Option<String>) + 'static) {
        let slot = resolver(on_choice);
        let on_select = move |model: Option<String>| resolve(&slot, model);
        let search: ComponentRef = HuggingFaceSearch::new(&self.tui, search, self.search_cache.clone(), on_select);
        let footer =
            format!("{} • {}", key_hint("tui.select.confirm", "select"), key_hint("tui.select.cancel", "back"));
        let content = frame("Download model", vec![spacer(1), search.clone()], Some(&footer));
        self.set_content(content, Some(search.clone()), Some(search));
    }

    /// A title and a muted message, without input.
    pub fn show_status(&mut self, title: &str, message: &str) {
        let body = vec![spacer(1), text(&fg(ThemeColor::Muted, message), 1, 0)];
        self.set_content(frame(title, body, None), None, None);
    }

    /// Shows `state`; `on_stop` runs when the user presses cancel (one press resolves every
    /// waiter added since the progress screen appeared).
    pub fn progress(&mut self, state: &ProgressState, on_stop: impl FnOnce() + 'static) {
        self.progress_waiters.push(Box::new(on_stop));
        self.showing_progress = true;
        self.update_progress(state);
    }

    /// Redraws the progress screen; ignored unless it is showing.
    pub fn update_progress(&mut self, state: &ProgressState) {
        if !self.showing_progress {
            return;
        }
        let progress = &state.progress;
        let mut body = vec![
            text(&fg(ThemeColor::Text, &state.model), 1, 0),
            spacer(1),
            text(&fg(ThemeColor::Muted, &progress.message), 1, 0),
        ];
        if let Some(ratio) = progress.ratio {
            body.push(text(&fg(ThemeColor::Accent, &progress_bar(ratio)), 1, 0));
        }
        if let Some(detail) = progress.detail.as_deref().filter(|detail| !detail.is_empty()) {
            body.push(text(&fg(ThemeColor::Dim, detail), 1, 0));
        }
        self.content = frame(&state.title, body, Some(&key_hint("tui.select.cancel", "stop")));
        self.input_handler = None;
        self.tui.request_render(false);
    }

    /// Whether the progress screen waits for a stop.
    pub fn is_waiting_for_stop(&self) -> bool {
        !self.progress_waiters.is_empty()
    }
}

impl Component for LlamaView {
    fn render(&mut self, width: usize) -> Vec<String> {
        clamp_lines(self.content.render(width), width)
    }

    fn handle_input(&mut self, data: &str) {
        if !self.progress_waiters.is_empty() && get_keybindings().matches(data, "tui.select.cancel") {
            for waiter in std::mem::take(&mut self.progress_waiters) {
                waiter();
            }
            return;
        }
        if let Some(handler) = &self.input_handler {
            handler.borrow_mut().handle_input(data);
        }
        self.tui.request_render(false);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn captures_viewport_input(&self) -> bool {
        self.input_handler.as_ref().is_some_and(|handler| handler.borrow().captures_viewport_input())
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if let Some(target) = &self.input_target {
            target.borrow_mut().set_focused(focused);
        }
    }

    fn invalidate(&mut self) {
        self.content.invalidate();
    }
}
