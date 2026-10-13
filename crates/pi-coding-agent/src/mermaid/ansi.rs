//! ANSI colouring of rendered art (upstream `ansi.ts`).

use std::borrow::Cow;

use super::types::{Cls, MermaidArt};

/// SGR parameters per semantic class, such as `"2"` (dim), `"36"` (cyan) or
/// `"38;5;244"` (a 256-colour index). A class left as `None` prints unstyled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnsiTheme {
    pub border: Option<Cow<'static, str>>,
    pub text: Option<Cow<'static, str>>,
    pub edge: Option<Cow<'static, str>>,
    pub edge_label: Option<Cow<'static, str>>,
    pub title: Option<Cow<'static, str>>,
    pub none: Option<Cow<'static, str>>,
}

/// Dim frame, plain labels, cyan connectors. Readable on light and dark backgrounds.
pub const DEFAULT_THEME: AnsiTheme = AnsiTheme {
    border: Some(Cow::Borrowed("2")),
    text: None,
    edge: Some(Cow::Borrowed("36")),
    edge_label: Some(Cow::Borrowed("2;36")),
    title: Some(Cow::Borrowed("1")),
    none: None,
};

impl Default for AnsiTheme {
    fn default() -> Self {
        DEFAULT_THEME
    }
}

impl AnsiTheme {
    /// The SGR parameters for `cls`, if the theme styles it.
    pub fn get(&self, cls: Cls) -> Option<&str> {
        let sgr = match cls {
            Cls::Border => &self.border,
            Cls::Text => &self.text,
            Cls::Edge => &self.edge,
            Cls::EdgeLabel => &self.edge_label,
            Cls::Title => &self.title,
            Cls::None => &self.none,
        };
        sgr.as_deref()
    }
}

/// Render art to ANSI-coloured lines, one per row of `art.styled`.
pub fn to_ansi(art: &MermaidArt, theme: &AnsiTheme) -> Vec<String> {
    art.styled
        .iter()
        .map(|row| {
            let mut line = String::new();
            for span in row {
                match theme.get(span.cls) {
                    Some(sgr) => {
                        line.push_str("\x1b[");
                        line.push_str(sgr);
                        line.push('m');
                        line.push_str(&span.text);
                        line.push_str("\x1b[0m");
                    }
                    None => line.push_str(&span.text),
                }
            }
            line
        })
        .collect()
}
