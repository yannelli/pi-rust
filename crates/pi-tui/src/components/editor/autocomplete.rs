//! Editor side of autocomplete: when to ask the provider, debouncing, stale results, and applying
//! a completion.

use std::rc::{Rc, Weak};
use std::time::Duration;

use super::Shared;
use super::popup::{AutocompleteList, SLASH_COMMAND_COLUMN_BOUNDS};
use super::state::{AutocompleteMode, DEFAULT_TRIGGER_CHARACTERS, EditorCore};
use super::task::{Latch, spawn_local};
use crate::autocomplete::{
    AbortController, AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, SuggestionOptions,
    is_autocomplete_separator,
};
use crate::ui_loop;
use crate::utils::{is_js_whitespace, is_whitespace_char, js_trim};

/// Debounce for symbol completions (`@`, `#`, provider triggers) while typing.
const SYMBOL_DEBOUNCE: Duration = Duration::from_millis(20);

/// Wrappers that may open a token in prose, like `(@src` or `` `@src ``.
const TOKEN_WRAPPERS: [char; 5] = ['(', '[', '{', '<', '`'];

#[derive(Clone, Copy, Debug)]
pub(super) struct RequestOptions {
    pub force: bool,
    pub explicit_tab: bool,
}

fn trim_start(text: &str) -> &str {
    text.trim_start_matches(is_js_whitespace)
}

/// True when `text` ends inside a trigger token: a boundary, optional wrappers, then `@"...` or a
/// trigger character followed by non-separators (upstream `buildTriggerPattern`).
pub(super) fn matches_trigger_context(text: &str, triggers: &[String]) -> bool {
    // A quoted attachment may contain separators: `@"my folder/`.
    if let Some(quote) = text.rfind('"')
        && text[..quote].ends_with('@')
    {
        let before_wrappers = text[..quote - 1].trim_end_matches(TOKEN_WRAPPERS);
        if before_wrappers.chars().next_back().is_none_or(is_autocomplete_separator) {
            return true;
        }
    }

    let token_start = text.char_indices().rfind(|(_, c)| is_autocomplete_separator(*c));
    let token = token_start.map_or(text, |(index, c)| &text[index + c.len_utf8()..]);
    for c in token.chars() {
        if triggers.iter().any(|trigger| trigger.chars().eq(std::iter::once(c))) {
            return true;
        }
        if !TOKEN_WRAPPERS.contains(&c) {
            break;
        }
    }
    false
}

impl EditorCore {
    pub fn set_autocomplete_provider(&mut self, provider: Rc<dyn AutocompleteProvider>) {
        self.cancel_autocomplete();
        let mut triggers: Vec<String> = DEFAULT_TRIGGER_CHARACTERS.iter().map(|c| c.to_string()).collect();
        for trigger in provider.trigger_characters() {
            let single = trigger.encode_utf16().count() == 1;
            if single && trigger != "/" && !is_whitespace_char(&trigger) && !triggers.contains(&trigger) {
                triggers.push(trigger);
            }
        }
        self.trigger_characters = triggers;
        self.provider = Some(provider);
    }

    pub fn matches_trigger_context(&self, text: &str) -> bool {
        matches_trigger_context(text, &self.trigger_characters)
    }

    /// The slash menu only opens on the first line.
    pub fn is_in_slash_command_context(&self, text_before_cursor: &str) -> bool {
        self.state.cursor_line == 0 && trim_start(text_before_cursor).starts_with('/')
    }

    pub fn is_at_start_of_message(&self) -> bool {
        if self.state.cursor_line != 0 {
            return false;
        }
        let before = js_trim(self.text_before_cursor());
        before.is_empty() || before == "/"
    }

    pub fn try_trigger_autocomplete(&mut self) {
        self.request_autocomplete(RequestOptions { force: false, explicit_tab: false });
    }

    /// Tab without an open popup: complete a command name, otherwise force path completion.
    pub fn handle_tab_completion(&mut self) {
        if self.provider.is_none() {
            return;
        }
        let before = self.text_before_cursor();
        let in_command_name = self.is_in_slash_command_context(before) && !trim_start(before).contains(' ');
        self.request_autocomplete(RequestOptions { force: !in_command_name, explicit_tab: true });
    }

    pub fn update_autocomplete(&mut self) {
        let Some(mode) = self.autocomplete_mode else { return };
        if self.provider.is_none() {
            return;
        }
        self.request_autocomplete(RequestOptions { force: mode == AutocompleteMode::Force, explicit_tab: false });
    }

    fn request_autocomplete(&mut self, options: RequestOptions) {
        let Some(provider) = self.provider.clone() else { return };
        if options.force
            && !provider.should_trigger_file_completion(
                &self.state.lines,
                self.state.cursor_line,
                self.state.cursor_col,
            )
        {
            return;
        }
        self.cancel_autocomplete_request();
        self.autocomplete_start_token += 1;
        let token = self.autocomplete_start_token;

        let debounce =
            !options.explicit_tab && !options.force && self.matches_trigger_context(self.text_before_cursor());
        if debounce {
            let weak = self.self_ref.clone();
            self.autocomplete_debounce = Some(ui_loop::set_timeout(SYMBOL_DEBOUNCE, move || {
                let Some(shared) = weak.upgrade() else { return };
                let Ok(mut core) = shared.core.try_borrow_mut() else { return };
                core.autocomplete_debounce = None;
                core.start_autocomplete_request(token, options);
            }));
            return;
        }
        self.start_autocomplete_request(token, options);
    }

    /// Queues a request behind the one still running, like upstream's chained request task.
    fn start_autocomplete_request(&mut self, token: u64, options: RequestOptions) {
        let running = self.autocomplete_latch.take();
        let latch = Latch::default();
        self.autocomplete_latch = Some(latch.clone());
        let weak = self.self_ref.clone();
        spawn_local(async move {
            if let Some(running) = running {
                running.wait().await;
            }
            run_request(weak, token, options).await;
            latch.set();
        });
    }

    fn finish_request(
        &mut self,
        request_id: u64,
        controller: &AbortController,
        snapshot: &(String, usize, usize),
        suggestions: Option<AutocompleteSuggestions>,
        options: RequestOptions,
    ) {
        let current = !controller.signal().is_aborted()
            && request_id == self.autocomplete_request_id
            && self.get_text() == snapshot.0
            && self.state.cursor_line == snapshot.1
            && self.state.cursor_col == snapshot.2;
        if !current {
            return;
        }
        self.autocomplete_abort = None;
        let Some(suggestions) = suggestions.filter(|s| !s.items.is_empty()) else {
            self.cancel_autocomplete();
            self.request_render();
            return;
        };
        if options.force && options.explicit_tab && suggestions.items.len() == 1 {
            self.apply_completion_item(&suggestions.items[0], &suggestions.prefix);
            self.request_render();
            return;
        }
        let mode = if options.force { AutocompleteMode::Force } else { AutocompleteMode::Regular };
        self.show_suggestions(suggestions, mode);
        self.request_render();
    }

    fn show_suggestions(&mut self, suggestions: AutocompleteSuggestions, mode: AutocompleteMode) {
        let bounds = suggestions.prefix.starts_with('/').then_some(SLASH_COMMAND_COLUMN_BOUNDS);
        let best = best_match_index(&suggestions.items, &suggestions.prefix);
        let theme = self.theme.select_list.clone();
        let mut list = AutocompleteList::new(suggestions.items, self.autocomplete_max_visible, theme, bounds);
        if let Some(index) = best {
            list.set_selected_index(index);
        }
        self.autocomplete_prefix = suggestions.prefix;
        self.autocomplete_list = Some(list);
        self.autocomplete_mode = Some(mode);
    }

    /// Applies `item` for `prefix` as one undo step.
    pub fn apply_completion_item(&mut self, item: &AutocompleteItem, prefix: &str) {
        let Some(provider) = self.provider.clone() else { return };
        self.push_undo_snapshot();
        self.last_action = None;
        let result =
            provider.apply_completion(&self.state.lines, self.state.cursor_line, self.state.cursor_col, item, prefix);
        self.state.lines = if result.lines.is_empty() { vec![String::new()] } else { result.lines };
        self.state.cursor_line = result.cursor_line.min(self.state.lines.len() - 1);
        let mut col = result.cursor_col.min(self.current_line().len());
        while !self.current_line().is_char_boundary(col) {
            col -= 1;
        }
        self.set_cursor_col(col);
        self.emit_change();
    }

    /// Applies the selected popup item and closes the popup.
    pub fn accept_completion(&mut self, item: &AutocompleteItem) {
        if self.provider.is_none() {
            return;
        }
        let prefix = self.autocomplete_prefix.clone();
        self.apply_completion_item(item, &prefix);
        self.cancel_autocomplete();
    }

    pub fn cancel_autocomplete_request(&mut self) {
        self.autocomplete_start_token += 1;
        if let Some(timer) = self.autocomplete_debounce.take() {
            ui_loop::clear_timer(timer);
        }
        if let Some(controller) = self.autocomplete_abort.take() {
            controller.abort();
        }
    }

    pub fn cancel_autocomplete(&mut self) {
        self.cancel_autocomplete_request();
        self.autocomplete_mode = None;
        self.autocomplete_list = None;
        self.autocomplete_prefix.clear();
    }
}

/// Exact value match first, then the first value starting with the prefix (case-sensitive).
fn best_match_index(items: &[AutocompleteItem], prefix: &str) -> Option<usize> {
    if prefix.is_empty() {
        return None;
    }
    items
        .iter()
        .position(|item| item.value == prefix)
        .or_else(|| items.iter().position(|item| item.value.starts_with(prefix)))
}

/// Calls the provider and applies its answer if the editor has not changed meanwhile.
async fn run_request(weak: Weak<Shared>, token: u64, options: RequestOptions) {
    let (controller, request_id, snapshot, pending) = {
        let Some(shared) = weak.upgrade() else { return };
        let Ok(mut core) = shared.core.try_borrow_mut() else { return };
        if token != core.autocomplete_start_token {
            return;
        }
        let Some(provider) = core.provider.clone() else { return };
        let controller = AbortController::new();
        core.autocomplete_abort = Some(controller.clone());
        core.autocomplete_request_id += 1;
        let snapshot = (core.get_text(), core.state.cursor_line, core.state.cursor_col);
        let pending = provider.get_suggestions(
            &core.state.lines,
            core.state.cursor_line,
            core.state.cursor_col,
            SuggestionOptions { signal: controller.signal(), force: options.force },
        );
        (controller, core.autocomplete_request_id, snapshot, pending)
    };
    let suggestions = pending.await;
    let Some(shared) = weak.upgrade() else { return };
    if let Ok(mut core) = shared.core.try_borrow_mut() {
        core.finish_request(request_id, &controller, &snapshot, suggestions, options);
    }
    shared.dispatch();
}
