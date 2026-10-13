//! Ports of the upstream editor autocomplete tests: slash commands, argument completion,
//! re-querying on cursor moves, and aborting stale requests.

mod editor_common;

use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use editor_common::{MockProvider, before_cursor, editor, flush, plain_lines, suggestions, type_text};
use pi_tui::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CombinedAutocompleteProvider, CompletionResult,
    SlashCommand, SuggestionOptions, SuggestionsFuture,
};
use pi_tui::ui_loop;

#[test]
fn requeries_when_the_cursor_moves_back_into_the_command_name() {
    let editor = editor();
    editor.set_autocomplete_provider(
        MockProvider::new(|lines, line, col, _| {
            let before = before_cursor(lines, line, col);
            if !before.starts_with('/') {
                return None;
            }
            match before.find(' ') {
                Some(space) => suggestions(&["repo", "message", "help"], &before[space + 1..]),
                None => suggestions(&["cmd"], &before),
            }
        })
        .shared(),
    );
    for c in "/cmd ".chars() {
        editor.handle_input(&c.to_string());
        flush();
    }
    assert_eq!(editor.get_text(), "/cmd ");
    assert!(editor.is_showing_autocomplete());
    assert!(plain_lines(&editor.render(80)).join("\n").contains("repo"));

    editor.handle_input("\x1b[D");
    flush();
    let after = plain_lines(&editor.render(80)).join("\n");
    assert!(!after.contains("repo"), "stale argument menu: {after}");
    assert!(!after.contains("message"), "stale argument menu: {after}");
}

/// Resolves after 500 ms unless aborted first, counting aborts.
struct SlowProvider {
    aborts: Arc<AtomicUsize>,
}

impl AutocompleteProvider for SlowProvider {
    fn get_suggestions(&self, _: &[String], _: usize, _: usize, options: SuggestionOptions) -> SuggestionsFuture {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<AutocompleteSuggestions>>();
        let sender = Arc::new(Mutex::new(Some(tx)));
        let on_timeout = sender.clone();
        let timer = ui_loop::set_timeout(Duration::from_millis(500), move || {
            if let Some(tx) = on_timeout.lock().unwrap().take() {
                let _ = tx.send(suggestions(&["@main.ts"], "@main"));
            }
        });
        let aborts = self.aborts.clone();
        options.signal.on_abort(move || {
            aborts.fetch_add(1, Ordering::SeqCst);
            if let Some(tx) = sender.lock().unwrap().take() {
                let _ = tx.send(None);
            }
        });
        Box::pin(async move {
            let result = rx.await.ok().flatten();
            ui_loop::clear_timer(timer);
            result
        })
    }

    fn apply_completion(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        editor_common::apply_completion(lines, line, col, item, prefix)
    }
}

#[test]
fn aborts_the_active_request_when_typing_continues() {
    let editor = editor();
    let aborts = Arc::new(AtomicUsize::new(0));
    editor.set_autocomplete_provider(Rc::new(SlowProvider { aborts: aborts.clone() }));
    type_text(&editor, "@mai");
    ui_loop::run_for(Duration::from_millis(250)).unwrap();
    editor.handle_input("n");
    ui_loop::run_for(Duration::from_millis(50)).unwrap();
    assert_eq!(aborts.load(Ordering::SeqCst), 1);
}

#[test]
fn hides_autocomplete_when_backspacing_the_slash() {
    let editor = editor();
    editor.set_autocomplete_provider(
        MockProvider::new(|lines, _, col, _| {
            let prefix = &lines[0][..col];
            let query = prefix.strip_prefix('/')?;
            let filtered: Vec<&str> = ["/model", "/help"].into_iter().filter(|c| c.starts_with(query)).collect();
            (!filtered.is_empty()).then(|| suggestions(&filtered, prefix)).flatten()
        })
        .shared(),
    );
    editor.handle_input("/");
    flush();
    assert_eq!(editor.get_text(), "/");
    assert!(editor.is_showing_autocomplete());
    editor.handle_input("\x7f");
    flush();
    assert_eq!(editor.get_text(), "");
    assert!(!editor.is_showing_autocomplete());
}

/// A provider for `/<command> <argument>` that offers `arguments`, filtered by prefix or not.
fn argument_provider(
    command: &'static str,
    arguments: &'static [&'static str],
    filter: bool,
) -> Rc<dyn AutocompleteProvider> {
    MockProvider::new(move |lines, _, col, _| {
        let before = &lines[0][..col];
        let rest = before.strip_prefix('/')?.strip_prefix(command)?;
        let argument = rest.trim_start();
        if rest.len() == argument.len() || argument.is_empty() || argument.contains(char::is_whitespace) {
            return None;
        }
        let matching: Vec<&str> =
            arguments.iter().copied().filter(|value| !filter || value.starts_with(argument)).collect();
        (!matching.is_empty()).then(|| suggestions(&matching, argument)).flatten()
    })
    .shared()
}

fn enter_after(provider: Rc<dyn AutocompleteProvider>, typed: &str) -> String {
    let editor = editor();
    editor.set_autocomplete_provider(provider);
    type_text(&editor, typed);
    assert_eq!(editor.get_text(), typed);
    flush();
    assert!(editor.is_showing_autocomplete(), "{typed:?}");
    editor.handle_input("\r");
    editor.get_text()
}

#[test]
fn enter_keeps_an_exact_typed_argument() {
    let provider = argument_provider("argtest", &["one", "two", "three"], true);
    assert_eq!(enter_after(provider, "/argtest two"), "/argtest two");
}

#[test]
fn enter_selects_the_first_prefix_match() {
    let provider = argument_provider("argtest", &["two", "three", "twelve"], true);
    assert_eq!(enter_after(provider, "/argtest t"), "/argtest two");
}

#[test]
fn enter_ignores_a_list_built_before_more_typing() {
    let editor = editor();
    editor.set_autocomplete_provider(argument_provider("argtest", &["two", "three"], true));
    type_text(&editor, "/argtest t");
    flush();
    assert!(editor.is_showing_autocomplete());
    let submitted = Rc::new(std::cell::RefCell::new(String::new()));
    let sink = submitted.clone();
    editor.on_submit(move |text| *sink.borrow_mut() = text.to_string());
    type_text(&editor, "x");
    editor.handle_input("\r");
    assert_eq!(*submitted.borrow(), "/argtest tx");
    assert!(!editor.is_showing_autocomplete());
}

#[test]
fn highlights_a_unique_prefix_match() {
    let provider = argument_provider("argtest", &["one", "two", "three"], false);
    assert_eq!(enter_after(provider, "/argtest tw"), "/argtest two");
}

#[test]
fn selects_the_first_of_several_prefix_matches() {
    let provider = argument_provider("argtest", &["one", "two", "three"], false);
    assert_eq!(enter_after(provider, "/argtest t"), "/argtest two");
}

#[test]
fn keeps_an_exact_model_like_argument() {
    let provider = argument_provider("model", &["gpt-4o", "gpt-4o-mini", "claude-sonnet"], true);
    assert_eq!(enter_after(provider, "/model gpt-4o-mini"), "/model gpt-4o-mini");
}

fn skills_provider(command: SlashCommand) -> Rc<dyn AutocompleteProvider> {
    Rc::new(CombinedAutocompleteProvider::new(vec![command.into()], ".", None))
}

#[test]
fn awaits_async_argument_completions() {
    let editor = editor();
    let command = SlashCommand::new("load-skills", Some("Load skills")).with_async_argument_completions(|prefix| {
        let items = prefix.starts_with('s').then(|| vec![AutocompleteItem::new("skill-a", "skill-a")]);
        Box::pin(async move { items })
    });
    editor.set_autocomplete_provider(skills_provider(command));
    editor.set_text("/load-skills ");
    editor.handle_input("s");
    flush();
    assert!(editor.is_showing_autocomplete());
    editor.handle_input("\t");
    assert_eq!(editor.get_text(), "/load-skills skill-a");
    assert!(!editor.is_showing_autocomplete());
}

/// Upstream feeds a non-array result; the Rust type only allows `None` or a list.
#[test]
fn ignores_empty_argument_completion_results() {
    let editor = editor();
    let command = SlashCommand::new("load-skills", Some("Load skills")).with_argument_completions(|_| Some(Vec::new()));
    editor.set_autocomplete_provider(skills_provider(command));
    editor.set_text("/load-skills ");
    editor.handle_input("s");
    flush();
    assert!(!editor.is_showing_autocomplete());
    assert_eq!(editor.get_text(), "/load-skills s");
}

#[test]
fn shows_no_argument_completions_without_a_completer() {
    let editor = editor();
    let commands = vec![
        SlashCommand::new("help", Some("Show help")).into(),
        SlashCommand::new("model", Some("Switch model"))
            .with_argument_completions(|_| Some(vec![AutocompleteItem::new("claude-opus", "claude-opus")]))
            .into(),
    ];
    editor.set_autocomplete_provider(Rc::new(CombinedAutocompleteProvider::new(commands, ".", None)));
    type_text(&editor, "/he");
    flush();
    assert!(editor.is_showing_autocomplete());
    editor.handle_input("\t");
    assert_eq!(editor.get_text(), "/help ");
    assert!(!editor.is_showing_autocomplete());
}

#[test]
fn renders_the_slash_menu_with_descriptions() {
    let editor = editor();
    let commands = vec![
        SlashCommand::new("help", Some("Show help")).into(),
        SlashCommand::new("model", Some("Switch model")).with_argument_hint("<id>").into(),
    ];
    editor.set_autocomplete_provider(Rc::new(CombinedAutocompleteProvider::new(commands, ".", None)));
    editor.handle_input("/");
    flush();
    let lines = plain_lines(&editor.render(60));
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[3].trim_end(), "\u{2192} help        Show help");
    assert_eq!(lines[4].trim_end(), "  model       <id> \u{2014} Switch model");
    editor.handle_input("\x1b[B");
    let lines = plain_lines(&editor.render(60));
    assert!(lines[4].starts_with("\u{2192} model"));
    editor.handle_input("\x1b");
    assert!(!editor.is_showing_autocomplete());
}
