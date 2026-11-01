//! Upstream `HuggingFaceSearch`: a debounced Hugging Face GGUF search with cached results,
//! fuzzy filtering of the shown results, and exact `owner/repository[:quant]` entry.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::time::Duration;

use futures::FutureExt;
use futures::future::LocalBoxFuture;
use pi_tui::components::{Input, InputOptions};
use pi_tui::ui_loop::{self, TimerId};
use pi_tui::{Component, Container, Tui, component, get_keybindings, get_select_page_index};
use tokio_util::sync::CancellationToken;

use super::super::common::{fg, fuzzy_filter_by, spacer, text};
use crate::llama::view::{
    NO_RESULTS_STATUS, SEARCH_DEBOUNCE_MS, SEARCH_HINT, SEARCH_MIN_CHARS_STATUS, SEARCHING_STATUS, compact_count,
    is_exact_model_query,
};
use crate::llama::{HuggingFaceClient, HuggingFaceModel};
use crate::theme::ThemeColor;

type SearchResult = Result<Vec<HuggingFaceModel>, String>;

/// Runs one search; the error text is shown in place of results.
pub type HuggingFaceSearchFn = Rc<dyn Fn(String, CancellationToken) -> LocalBoxFuture<'static, SearchResult>>;

/// Results by lowercased query, kept for the lifetime of the `/llama` view.
pub type SearchCache = Rc<RefCell<HashMap<String, Vec<HuggingFaceModel>>>>;

/// A [`HuggingFaceSearchFn`] backed by `client`.
pub fn hugging_face_search_fn(client: HuggingFaceClient) -> HuggingFaceSearchFn {
    let client = Rc::new(client);
    Rc::new(move |query: String, cancel: CancellationToken| {
        let client = client.clone();
        async move { client.search(&query, &cancel).await.map_err(|error| error.to_string()) }.boxed_local()
    })
}

const MAX_VISIBLE: usize = 10;

pub struct HuggingFaceSearch {
    this: Weak<RefCell<Self>>,
    tui: Tui,
    search: HuggingFaceSearchFn,
    cache: SearchCache,
    on_select: Option<Box<dyn FnOnce(Option<String>)>>,
    container: Container,
    input: Rc<RefCell<Input>>,
    results_container: Rc<RefCell<Container>>,
    results: Vec<HuggingFaceModel>,
    filtered: Vec<HuggingFaceModel>,
    selected: usize,
    query: String,
    status: String,
    debounce: Option<TimerId>,
    request: Option<(u64, CancellationToken)>,
    requests: u64,
    closed: bool,
}

impl HuggingFaceSearch {
    /// `on_select` runs once, with the chosen model or `None` when cancelled. Searches run on
    /// `spawn_local` tasks, so the view needs a `LocalSet`.
    pub fn new(
        tui: &Tui,
        search: HuggingFaceSearchFn,
        cache: SearchCache,
        on_select: impl FnOnce(Option<String>) + 'static,
    ) -> Rc<RefCell<Self>> {
        let input = component(Input::new(InputOptions::default()));
        let results_container = component(Container::new());
        let mut container = Container::new();
        container.add_child(text(&fg(ThemeColor::Dim, SEARCH_HINT), 1, 0));
        container.add_child(input.clone());
        container.add_child(spacer(1));
        container.add_child(results_container.clone());
        let search = Rc::new_cyclic(|this| {
            RefCell::new(Self {
                this: this.clone(),
                tui: tui.clone(),
                search,
                cache,
                on_select: Some(Box::new(on_select)),
                container,
                input,
                results_container,
                results: Vec::new(),
                filtered: Vec::new(),
                selected: 0,
                query: String::new(),
                status: SEARCH_MIN_CHARS_STATUS.to_string(),
                debounce: None,
                request: None,
                requests: 0,
                closed: false,
            })
        });
        search.borrow_mut().update_results();
        search
    }

    /// The trimmed query.
    pub fn query(&self) -> &str {
        &self.query
    }

    fn update_results(&mut self) {
        let mut results = self.results_container.borrow_mut();
        results.clear();
        let len = self.filtered.len();
        let start = (self.selected as i64 - (MAX_VISIBLE / 2) as i64).min(len as i64 - MAX_VISIBLE as i64).max(0);
        let start = start as usize;
        let end = (start + MAX_VISIBLE).min(len);
        for (index, model) in self.filtered.iter().enumerate().take(end).skip(start) {
            let details = format!("{} downloads", compact_count(model.downloads));
            let line = if index == self.selected {
                fg(ThemeColor::Accent, &format!("→ {}  {details}", model.id))
            } else {
                format!("  {}{}", model.id, fg(ThemeColor::Muted, &format!("  {details}")))
            };
            results.add_child(text(&line, 0, 0));
        }
        if start > 0 || end < len {
            results.add_child(text(&fg(ThemeColor::Dim, &format!("  ({}/{len})", self.selected + 1)), 0, 0));
        }
        if len == 0 || self.status == SEARCHING_STATUS {
            results.add_child(text(&fg(ThemeColor::Dim, &format!("  {}", self.status)), 0, 0));
        }
        self.tui.request_render(false);
    }

    fn filter_results(&mut self) {
        self.filtered = if self.query.is_empty() {
            self.results.clone()
        } else {
            let matches: Vec<String> = fuzzy_filter_by(&self.results, &self.query, |model| model.id.clone())
                .into_iter()
                .map(|m| m.id)
                .collect();
            self.results.iter().filter(|model| matches.contains(&model.id)).cloned().collect()
        };
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.update_results();
    }

    fn clear_pending(&mut self) {
        if let Some(timer) = self.debounce.take() {
            ui_loop::clear_timer(timer);
        }
        if let Some((_, request)) = self.request.take() {
            request.cancel();
        }
    }

    fn schedule_search(&mut self) {
        self.clear_pending();
        if self.query.encode_utf16().count() < 2 {
            self.status = SEARCH_MIN_CHARS_STATUS.to_string();
            self.filter_results();
            return;
        }
        let cached = self.cache.borrow().get(&self.query.to_lowercase()).cloned();
        if let Some(cached) = cached {
            self.status = if cached.is_empty() { NO_RESULTS_STATUS } else { "" }.to_string();
            self.results = cached;
            self.filter_results();
            return;
        }
        self.status = SEARCHING_STATUS.to_string();
        self.filter_results();
        let this = self.this.clone();
        self.debounce = Some(ui_loop::set_timeout(Duration::from_millis(SEARCH_DEBOUNCE_MS), move || {
            if let Some(this) = this.upgrade() {
                this.borrow_mut().run_search();
            }
        }));
    }

    fn run_search(&mut self) {
        self.debounce = None;
        let request = CancellationToken::new();
        self.requests += 1;
        let id = self.requests;
        self.request = Some((id, request.clone()));
        let (query, this) = (self.query.clone(), self.this.clone());
        let search = (self.search)(query.clone(), request.clone());
        tokio::task::spawn_local(async move {
            let result = search.await;
            if let Some(this) = this.upgrade() {
                this.borrow_mut().finish_search(&query, id, &request, result);
            }
        });
    }

    fn finish_search(&mut self, query: &str, id: u64, request: &CancellationToken, result: SearchResult) {
        if let Ok(results) = &result {
            self.cache.borrow_mut().insert(query.to_lowercase(), results.clone());
        }
        if self.request.as_ref().is_some_and(|(active, _)| *active == id) {
            self.request = None;
        }
        if self.closed || request.is_cancelled() || self.query != query {
            return;
        }
        match result {
            Ok(results) => {
                self.status = if results.is_empty() { NO_RESULTS_STATUS } else { "" }.to_string();
                self.results = results;
                self.selected = 0;
            }
            Err(message) => {
                self.results = Vec::new();
                self.status = message;
            }
        }
        self.filter_results();
    }

    fn close(&mut self, model: Option<String>) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.clear_pending();
        if let Some(on_select) = self.on_select.take() {
            on_select(model);
        }
    }
}

impl Component for HuggingFaceSearch {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        let keybindings = get_keybindings();
        if keybindings.matches(data, "tui.select.up") {
            if !self.filtered.is_empty() {
                self.selected = if self.selected == 0 { self.filtered.len() - 1 } else { self.selected - 1 };
                self.update_results();
            }
            return;
        }
        if keybindings.matches(data, "tui.select.down") {
            if !self.filtered.is_empty() {
                self.selected = if self.selected + 1 == self.filtered.len() { 0 } else { self.selected + 1 };
                self.update_results();
            }
            return;
        }
        if let Some(page_index) =
            get_select_page_index(&keybindings, data, self.selected, self.filtered.len(), MAX_VISIBLE)
        {
            if !self.filtered.is_empty() {
                self.selected = page_index;
                self.update_results();
            }
            return;
        }
        if keybindings.matches(data, "tui.select.confirm") {
            let exact = is_exact_model_query(&self.query).then(|| self.query.clone());
            let selected = exact.or_else(|| self.filtered.get(self.selected).map(|model| model.id.clone()));
            if selected.is_some() {
                self.close(selected);
            }
            return;
        }
        if keybindings.matches(data, "tui.select.cancel") {
            self.close(None);
            return;
        }
        drop(keybindings);
        self.input.borrow_mut().handle_input(data);
        let query = self.input.borrow().get_value().trim().to_string();
        if query == self.query {
            return;
        }
        self.query = query;
        self.schedule_search();
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn captures_viewport_input(&self) -> bool {
        true
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.input.borrow_mut().set_focused(focused);
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
