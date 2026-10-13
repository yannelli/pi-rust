//! Syntax highlighting for code blocks and file previews, ported from upstream `utils/syntax-highlight.ts`.
//! Upstream uses highlight.js; this uses syntect grammars and maps their scopes to highlight.js classes,
//! so a [`HighlightTheme`] keyed by highlight.js class names colors code the same way.

mod languages;
mod scopes;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use syntect::easy::ScopeRangeIterator;
use syntect::parsing::{ParseState, ScopeStack};
use syntect::util::LinesWithEndings;

pub use languages::supported_languages;
use languages::{SYNTAX_SET, resolve_syntax};
use scopes::{Family, classify};

/// Formats one run of text (one line at most).
pub type HighlightFormatter = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Formatters keyed by highlight.js class (`keyword`, `string`, `title`, ...). `default` formats text
/// outside any class; without it, such text is left as is.
#[derive(Clone, Default)]
pub struct HighlightTheme {
    formatters: HashMap<String, HighlightFormatter>,
}

impl fmt::Debug for HighlightTheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut keys: Vec<&String> = self.formatters.keys().collect();
        keys.sort();
        f.debug_struct("HighlightTheme").field("classes", &keys).finish()
    }
}

impl HighlightTheme {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace the formatter for a class.
    pub fn with(mut self, class: &str, formatter: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        self.formatters.insert(class.to_string(), Arc::new(formatter));
        self
    }

    pub fn insert(&mut self, class: &str, formatter: HighlightFormatter) {
        self.formatters.insert(class.to_string(), formatter);
    }

    /// The formatter for a class: exact, else the part before the first `.`, else before the first `-`.
    pub fn formatter(&self, class: &str) -> Option<&HighlightFormatter> {
        if let Some(exact) = self.formatters.get(class) {
            return Some(exact);
        }
        let prefix = |separator: char| class.split_once(separator).and_then(|(head, _)| self.formatters.get(head));
        prefix('.').or_else(|| prefix('-'))
    }

    fn active(&self, class: Option<&str>) -> Option<&HighlightFormatter> {
        class.and_then(|class| self.formatter(class)).or_else(|| self.formatters.get("default"))
    }
}

/// Options for [`highlight`].
#[derive(Debug, Clone, Copy, Default)]
pub struct HighlightOptions<'a> {
    /// The language name or alias. Without one, the language is detected from a shebang line.
    pub language: Option<&'a str>,
    /// Accepted for parity with upstream; grammars here never reject input.
    pub ignore_illegals: bool,
    pub theme: Option<&'a HighlightTheme>,
}

/// Why code could not be highlighted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightError(pub String);

impl fmt::Display for HighlightError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HighlightError {}

/// Whether a language name or alias can be highlighted (upstream `supportsLanguage`).
pub fn supports_language(name: &str) -> bool {
    resolve_syntax(name).is_some()
}

/// Load the grammars in the background so the first highlighted block does not wait for them.
pub fn load_all_highlight_languages() {
    let _ = std::thread::Builder::new().name("pi-highlight-load".into()).spawn(|| {
        let _ = SYNTAX_SET.syntaxes().len();
    });
}

/// A run of source text and the highlight.js class it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightToken {
    pub text: String,
    pub class: Option<&'static str>,
}

/// Split code into classified runs; consecutive runs with the same class are merged.
pub fn tokenize(code: &str, language: Option<&str>) -> Result<Vec<HighlightToken>, HighlightError> {
    let syntax = match language {
        Some(language) => {
            resolve_syntax(language).ok_or_else(|| HighlightError(format!("Unknown language: \"{language}\"")))?
        }
        None => code
            .lines()
            .next()
            .and_then(|line| SYNTAX_SET.find_syntax_by_first_line(line))
            .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text()),
    };
    let family = Family::for_syntax(&syntax.name);
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut tokens: Vec<HighlightToken> = Vec::new();
    for (index, line) in LinesWithEndings::from(code).enumerate() {
        let ops = state.parse_line(line, &SYNTAX_SET).map_err(|error| HighlightError(error.to_string()))?;
        for (range, op) in ScopeRangeIterator::new(&ops, line) {
            stack.apply(op).map_err(|error| HighlightError(format!("{error:?}")))?;
            if range.is_empty() {
                continue;
            }
            let text = &line[range];
            let mut class = classify(stack.as_slice(), text, family);
            // highlight.js marks a whole shebang line as `meta`.
            if index == 0 && line.starts_with("#!") {
                class = Some("meta");
            }
            // highlight.js leaves member access (`JSON.parse`, `System.out`) plain.
            if class == Some("built_in") && tokens.last().is_some_and(|last| last.text.ends_with('.')) {
                class = None;
            }
            match tokens.last_mut() {
                Some(last) if last.class == class => last.text.push_str(text),
                _ => tokens.push(HighlightToken { text: text.to_string(), class }),
            }
        }
    }
    Ok(tokens)
}

/// Format classified runs: each non-empty line of a run goes through its class's formatter.
pub fn render_tokens(tokens: &[HighlightToken], theme: &HighlightTheme) -> String {
    let mut output = String::new();
    for token in tokens {
        match theme.active(token.class) {
            Some(formatter) => {
                let lines: Vec<String> = token
                    .text
                    .split('\n')
                    .map(|line| if line.is_empty() { String::new() } else { formatter(line) })
                    .collect();
                output.push_str(&lines.join("\n"));
            }
            None => output.push_str(&token.text),
        }
    }
    output
}

/// Highlight code with a theme of highlight.js class formatters (upstream `highlight`).
pub fn highlight(code: &str, options: &HighlightOptions<'_>) -> Result<String, HighlightError> {
    let tokens = tokenize(code, options.language)?;
    let empty = HighlightTheme::default();
    Ok(render_tokens(&tokens, options.theme.unwrap_or(&empty)))
}

#[cfg(test)]
mod tests;
