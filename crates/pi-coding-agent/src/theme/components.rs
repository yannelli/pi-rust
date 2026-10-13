//! Theme helpers for TUI components: code highlighting and the Markdown, select list, editor, and
//! settings list themes. Every style function reads the active theme when called, like upstream.

use std::rc::Rc;
use std::sync::{Arc, Mutex};

use super::registry::theme;
use super::theme::Theme;
use super::tokens::ThemeColor;
use super::tui_themes::{EditorTheme, MarkdownTheme, SelectListTheme, SettingsListTheme, StyleFn};
use crate::highlight::{HighlightOptions, HighlightTheme, highlight, supports_language};

/// How the CLI highlight theme styles one highlight.js class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightStyle {
    Fg(ThemeColor),
    Bold,
    Italic,
    Underline,
}

/// The highlight.js classes the CLI theme styles (upstream `buildCliHighlightTheme`).
pub const CLI_HIGHLIGHT_CLASSES: &[(&str, HighlightStyle)] = {
    use HighlightStyle::*;
    use ThemeColor::*;
    &[
        ("keyword", Fg(SyntaxKeyword)),
        ("built_in", Fg(SyntaxType)),
        ("literal", Fg(SyntaxNumber)),
        ("number", Fg(SyntaxNumber)),
        ("regexp", Fg(SyntaxString)),
        ("string", Fg(SyntaxString)),
        ("subst", Fg(Text)),
        ("comment", Fg(SyntaxComment)),
        ("doctag", Fg(SyntaxComment)),
        ("meta", Fg(Muted)),
        ("function", Fg(SyntaxFunction)),
        ("title", Fg(SyntaxFunction)),
        ("class", Fg(SyntaxType)),
        ("type", Fg(SyntaxType)),
        ("tag", Fg(SyntaxPunctuation)),
        ("name", Fg(SyntaxKeyword)),
        ("attr", Fg(SyntaxVariable)),
        ("variable", Fg(SyntaxVariable)),
        ("params", Fg(SyntaxVariable)),
        ("operator", Fg(SyntaxOperator)),
        ("punctuation", Fg(SyntaxPunctuation)),
        ("emphasis", Italic),
        ("strong", Bold),
        ("link", Underline),
        ("addition", Fg(ToolDiffAdded)),
        ("deletion", Fg(ToolDiffRemoved)),
    ]
};

/// The highlight theme that colors code with a theme's syntax tokens.
pub fn cli_highlight_theme(theme: &Arc<Theme>) -> HighlightTheme {
    let mut highlight_theme = HighlightTheme::new();
    for &(class, style) in CLI_HIGHLIGHT_CLASSES {
        let theme = theme.clone();
        highlight_theme.insert(
            class,
            Arc::new(move |text: &str| match style {
                HighlightStyle::Fg(color) => theme.fg(color, text),
                HighlightStyle::Bold => theme.bold(text),
                HighlightStyle::Italic => theme.italic(text),
                HighlightStyle::Underline => theme.underline(text),
            }),
        );
    }
    highlight_theme
}

type CachedHighlightTheme = Option<(Arc<Theme>, HighlightTheme)>;
static CACHED_HIGHLIGHT_THEME: Mutex<CachedHighlightTheme> = Mutex::new(None);

fn cached_highlight_theme(theme: &Arc<Theme>) -> HighlightTheme {
    let mut cache = CACHED_HIGHLIGHT_THEME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match cache.as_ref() {
        Some((cached, highlight_theme)) if Arc::ptr_eq(cached, theme) => highlight_theme.clone(),
        _ => {
            let highlight_theme = cli_highlight_theme(theme);
            *cache = Some((theme.clone(), highlight_theme.clone()));
            highlight_theme
        }
    }
}

/// Highlight with the active theme. `None` when the language is not supported: upstream skips
/// auto-detection because it misidentifies prose.
fn highlight_with_active_theme(code: &str, lang: Option<&str>) -> Option<Result<Vec<String>, ()>> {
    let lang = lang.filter(|lang| supports_language(lang))?;
    let theme = theme();
    let highlight_theme = cached_highlight_theme(&theme);
    let options = HighlightOptions { language: Some(lang), ignore_illegals: true, theme: Some(&highlight_theme) };
    Some(highlight(code, &options).map(|text| text.split('\n').map(String::from).collect()).map_err(|_| ()))
}

fn code_block_lines(code: &str) -> Vec<String> {
    let theme = theme();
    code.split('\n').map(|line| theme.fg(ThemeColor::MdCodeBlock, line)).collect()
}

/// Highlight code for a language (upstream `highlightCode`); lines in the code block color otherwise.
pub fn highlight_code(code: &str, lang: Option<&str>) -> Vec<String> {
    match highlight_with_active_theme(code, lang) {
        None => code_block_lines(code),
        Some(Ok(lines)) => lines,
        Some(Err(())) => code.split('\n').map(String::from).collect(),
    }
}

/// The highlight language for a file path's extension (upstream `getLanguageFromPath`).
pub fn get_language_from_path(path: &str) -> Option<&'static str> {
    let extension = path.rsplit('.').next()?.to_lowercase();
    let language = match extension.as_str() {
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "fish" => "fish",
        "ps1" => "powershell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "md" | "markdown" => "markdown",
        "dockerfile" => "dockerfile",
        "makefile" => "makefile",
        "cmake" => "cmake",
        "lua" => "lua",
        "perl" => "perl",
        "r" => "r",
        "scala" => "scala",
        "clj" => "clojure",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "hs" => "haskell",
        "ml" => "ocaml",
        "vim" => "vim",
        "graphql" => "graphql",
        "proto" => "protobuf",
        "tf" | "hcl" => "hcl",
        _ => return None,
    };
    Some(language)
}

fn fg(color: ThemeColor) -> StyleFn {
    Rc::new(move |text: &str| theme().fg(color, text))
}

/// The Markdown component theme (upstream `getMarkdownTheme`).
pub fn get_markdown_theme() -> MarkdownTheme {
    use ThemeColor::*;
    MarkdownTheme {
        heading: fg(MdHeading),
        link: fg(MdLink),
        link_url: fg(MdLinkUrl),
        code: fg(MdCode),
        code_block: fg(MdCodeBlock),
        code_block_border: fg(MdCodeBlockBorder),
        quote: fg(MdQuote),
        quote_border: fg(MdQuoteBorder),
        hr: fg(MdHr),
        list_bullet: fg(MdListBullet),
        bold: Rc::new(|text: &str| theme().bold(text)),
        italic: Rc::new(|text: &str| theme().italic(text)),
        underline: Rc::new(|text: &str| theme().underline(text)),
        strikethrough: Rc::new(|text: &str| theme().strikethrough(text)),
        highlight_code: Some(Rc::new(|code: &str, lang: Option<&str>| match highlight_with_active_theme(code, lang) {
            Some(Ok(lines)) => lines,
            _ => code_block_lines(code),
        })),
        code_block_indent: None,
    }
}

/// The SelectList component theme (upstream `getSelectListTheme`).
pub fn get_select_list_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: fg(ThemeColor::Accent),
        selected_text: fg(ThemeColor::Accent),
        description: fg(ThemeColor::Muted),
        scroll_info: fg(ThemeColor::Muted),
        no_match: fg(ThemeColor::Muted),
    }
}

/// The Editor component theme (upstream `getEditorTheme`).
pub fn get_editor_theme() -> EditorTheme {
    EditorTheme { border_color: fg(ThemeColor::BorderMuted), select_list: get_select_list_theme() }
}

/// The SettingsList component theme (upstream `getSettingsListTheme`). The cursor is styled once.
pub fn get_settings_list_theme() -> SettingsListTheme {
    SettingsListTheme {
        label: Rc::new(
            |text: &str, selected: bool| {
                if selected { theme().fg(ThemeColor::Accent, text) } else { text.to_string() }
            },
        ),
        value: Rc::new(|text: &str, selected: bool| {
            theme().fg(if selected { ThemeColor::Accent } else { ThemeColor::Muted }, text)
        }),
        description: fg(ThemeColor::Dim),
        cursor: theme().fg(ThemeColor::Accent, "→ "),
        hint: fg(ThemeColor::Dim),
    }
}
