//! Helpers shared by the `CombinedAutocompleteProvider` test ports.
#![allow(dead_code)]

use std::future::Future;
use std::path::Path;

use pi_tui::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CombinedAutocompleteProvider, CompletionResult,
    SuggestionOptions,
};

/// Runs a provider future to completion on this thread.
pub fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(future)
}

/// Upstream `getSuggestions(provider, lines, line, col, force)`.
pub fn suggest(
    provider: &CombinedAutocompleteProvider,
    line: &str,
    cursor_col: usize,
    force: bool,
) -> Option<AutocompleteSuggestions> {
    let lines = vec![line.to_string()];
    let options = SuggestionOptions { force, ..SuggestionOptions::default() };
    block_on(provider.get_suggestions(&lines, 0, cursor_col, options))
}

pub fn apply(
    provider: &CombinedAutocompleteProvider,
    line: &str,
    cursor_col: usize,
    item: &AutocompleteItem,
    prefix: &str,
) -> CompletionResult {
    provider.apply_completion(&[line.to_string()], 0, cursor_col, item, prefix)
}

pub fn values(result: &Option<AutocompleteSuggestions>) -> Vec<String> {
    result.as_ref().map(|r| r.items.iter().map(|item| item.value.clone()).collect()).unwrap_or_default()
}

/// Upstream `setupFolder`: creates directories and files under `base`.
pub fn setup_folder(base: &Path, dirs: &[&str], files: &[(&str, &str)]) {
    for dir in dirs {
        std::fs::create_dir_all(base.join(dir)).expect("create dir");
    }
    for (path, contents) in files {
        let full = base.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(full, contents).expect("write file");
    }
}

pub fn provider(base: &Path) -> CombinedAutocompleteProvider {
    CombinedAutocompleteProvider::new(Vec::new(), base.to_string_lossy().into_owned(), None)
}
