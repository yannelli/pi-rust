//! Autocomplete (upstream `autocomplete.ts`): providers, slash commands, paths, and `@` file search.
//!
//! Providers run on the UI thread and return a local future that the editor polls from the UI loop.

mod abort;
mod combined;
mod fd;
mod files;
mod paths;

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

pub use abort::{AbortController, AbortSignal};
pub use combined::CombinedAutocompleteProvider;
pub(crate) use paths::is_autocomplete_separator;

/// A boxed future that runs on the UI thread.
pub type LocalBoxFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// Result of [`AutocompleteProvider::get_suggestions`].
pub type SuggestionsFuture = LocalBoxFuture<Option<AutocompleteSuggestions>>;

/// One suggestion. Same shape as a select list item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutocompleteItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

impl AutocompleteItem {
    /// An item without a description.
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self { value: value.into(), label: label.into(), description: None }
    }

    /// Sets the description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Suggestions and the text they complete.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutocompleteSuggestions {
    pub items: Vec<AutocompleteItem>,
    /// What the items match against (for example `/` or `src/`). It ends at the cursor.
    pub prefix: String,
}

/// Editor text and cursor after a completion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompletionResult {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    /// UTF-8 byte offset in `lines[cursor_line]`, like every cursor column in this module.
    pub cursor_col: usize,
}

/// Options for [`AutocompleteProvider::get_suggestions`].
#[derive(Clone, Debug, Default)]
pub struct SuggestionOptions {
    /// Aborted when the editor no longer needs the result.
    pub signal: AbortSignal,
    /// Set for an explicit Tab request, which completes paths anywhere.
    pub force: bool,
}

/// Source of completions for the editor. An application or extension can wrap a provider to add
/// its own (see [`wrap_autocomplete_provider`]).
pub trait AutocompleteProvider {
    /// Characters that trigger this provider at token boundaries, besides `@` and `#`.
    fn trigger_characters(&self) -> Vec<String> {
        Vec::new()
    }

    /// Suggestions for the cursor position, or `None` when there are none. The future must not
    /// borrow the editor; copy what it needs from `lines`.
    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: SuggestionOptions,
    ) -> SuggestionsFuture;

    /// Applies the selected item and returns the new text and cursor.
    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult;

    /// Whether an explicit Tab should request file completion here. Defaults to true.
    fn should_trigger_file_completion(&self, _lines: &[String], _cursor_line: usize, _cursor_col: usize) -> bool {
        true
    }
}

/// Result of [`SlashCommand::get_argument_completions`].
pub type ArgumentCompletionsFuture = LocalBoxFuture<Option<Vec<AutocompleteItem>>>;

/// Completes the argument text of a slash command (everything after the first space).
pub type ArgumentCompleter = Rc<dyn Fn(&str) -> ArgumentCompletionsFuture>;

/// A slash command offered by [`CombinedAutocompleteProvider`].
#[derive(Clone, Default)]
pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
    /// Shown before the description, for example `<model>`.
    pub argument_hint: Option<String>,
    /// Argument completions; `None` when the command takes no completable argument.
    pub get_argument_completions: Option<ArgumentCompleter>,
}

impl SlashCommand {
    /// A command with a name and an optional description.
    pub fn new(name: impl Into<String>, description: Option<&str>) -> Self {
        Self { name: name.into(), description: description.map(str::to_string), ..Self::default() }
    }

    /// Sets the argument hint.
    pub fn with_argument_hint(mut self, hint: impl Into<String>) -> Self {
        self.argument_hint = Some(hint.into());
        self
    }

    /// Sets an argument completer that answers right away.
    pub fn with_argument_completions(
        mut self,
        completer: impl Fn(&str) -> Option<Vec<AutocompleteItem>> + 'static,
    ) -> Self {
        self.get_argument_completions = Some(Rc::new(move |prefix: &str| -> ArgumentCompletionsFuture {
            let items = completer(prefix);
            Box::pin(std::future::ready(items))
        }));
        self
    }

    /// Sets an argument completer that returns a future.
    pub fn with_async_argument_completions(
        mut self,
        completer: impl Fn(&str) -> ArgumentCompletionsFuture + 'static,
    ) -> Self {
        self.get_argument_completions = Some(Rc::new(completer));
        self
    }
}

impl std::fmt::Debug for SlashCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlashCommand")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("argument_hint", &self.argument_hint)
            .field("has_argument_completions", &self.get_argument_completions.is_some())
            .finish()
    }
}

/// A command entry: a full [`SlashCommand`] or a plain item whose value is the command name.
#[derive(Clone, Debug)]
pub enum CommandEntry {
    Command(SlashCommand),
    Item(AutocompleteItem),
}

impl CommandEntry {
    /// The command name.
    pub fn name(&self) -> &str {
        match self {
            CommandEntry::Command(command) => &command.name,
            CommandEntry::Item(item) => &item.value,
        }
    }
}

impl From<SlashCommand> for CommandEntry {
    fn from(command: SlashCommand) -> Self {
        CommandEntry::Command(command)
    }
}

impl From<AutocompleteItem> for CommandEntry {
    fn from(item: AutocompleteItem) -> Self {
        CommandEntry::Item(item)
    }
}

/// Wraps the current provider in a new one (upstream extension `addAutocompleteProvider`).
pub type AutocompleteProviderFactory = Rc<dyn Fn(Rc<dyn AutocompleteProvider>) -> Rc<dyn AutocompleteProvider>>;

/// Applies each factory to `base`, first to last. When any wrapper declares trigger characters,
/// the result reports their union, like the upstream interactive mode.
pub fn wrap_autocomplete_provider(
    base: Rc<dyn AutocompleteProvider>,
    factories: &[AutocompleteProviderFactory],
) -> Rc<dyn AutocompleteProvider> {
    let mut provider = base;
    let mut triggers: Vec<String> = Vec::new();
    for factory in factories {
        provider = factory(provider);
        for trigger in provider.trigger_characters() {
            if !triggers.contains(&trigger) {
                triggers.push(trigger);
            }
        }
    }
    if triggers.is_empty() { provider } else { Rc::new(WithTriggerCharacters { inner: provider, triggers }) }
}

/// A provider with its trigger characters replaced.
pub struct WithTriggerCharacters {
    pub inner: Rc<dyn AutocompleteProvider>,
    pub triggers: Vec<String>,
}

impl AutocompleteProvider for WithTriggerCharacters {
    fn trigger_characters(&self) -> Vec<String> {
        self.triggers.clone()
    }

    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: SuggestionOptions,
    ) -> SuggestionsFuture {
        self.inner.get_suggestions(lines, cursor_line, cursor_col, options)
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        self.inner.apply_completion(lines, cursor_line, cursor_col, item, prefix)
    }

    fn should_trigger_file_completion(&self, lines: &[String], cursor_line: usize, cursor_col: usize) -> bool {
        self.inner.should_trigger_file_completion(lines, cursor_line, cursor_col)
    }
}
