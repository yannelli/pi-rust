//! Helpers for the ports of upstream `markdown.test.ts`: a chalk-compatible theme, ANSI
//! stripping, and a main-screen TUI on a virtual terminal for cell checks.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::MutexGuard;
use std::time::Duration;

use pi_tui::components::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme, StyleFn};
use pi_tui::env::MapEnv;
use pi_tui::terminal_image::{TerminalCapabilities, reset_capabilities_cache, set_capabilities};
use pi_tui::testing::VirtualTerminal;
use pi_tui::{ComponentRef, Tui, TuiOptions, TuiStopOptions, ui_loop};

pub const BOLD: (u8, u8) = (1, 22);
pub const DIM: (u8, u8) = (2, 22);
pub const ITALIC: (u8, u8) = (3, 23);
pub const UNDERLINE: (u8, u8) = (4, 24);
pub const STRIKETHROUGH: (u8, u8) = (9, 29);
pub const RED: (u8, u8) = (31, 39);
pub const GREEN: (u8, u8) = (32, 39);
pub const YELLOW: (u8, u8) = (33, 39);
pub const BLUE: (u8, u8) = (34, 39);
pub const MAGENTA: (u8, u8) = (35, 39);
pub const CYAN: (u8, u8) = (36, 39);
pub const GRAY: (u8, u8) = (90, 39);

/// chalk (level 3) `applyStyle` for a chain such as `chalk.bold.cyan`: closes inside the text
/// reopen the style, and line breaks close and reopen it.
pub fn chalk_apply(styles: &[(u8, u8)], text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let open_all: String = styles.iter().map(|(open, _)| format!("\x1b[{open}m")).collect();
    let close_all: String = styles.iter().rev().map(|(_, close)| format!("\x1b[{close}m")).collect();
    let mut text = text.to_string();
    if text.contains('\x1b') {
        for (open, close) in styles.iter().rev() {
            let close = format!("\x1b[{close}m");
            text = text.replace(&close, &format!("{close}\x1b[{open}m"));
        }
    }
    if text.contains('\n') {
        text = text.replace('\n', &format!("{close_all}\n{open_all}"));
    }
    format!("{open_all}{text}{close_all}")
}

pub fn chalk(styles: &'static [(u8, u8)]) -> StyleFn {
    Rc::new(move |text: &str| chalk_apply(styles, text))
}

/// Upstream `defaultMarkdownTheme` from `test-themes.ts`.
pub fn default_theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: chalk(&[BOLD, CYAN]),
        link: chalk(&[BLUE]),
        link_url: chalk(&[DIM]),
        code: chalk(&[YELLOW]),
        code_block: chalk(&[GREEN]),
        code_block_border: chalk(&[DIM]),
        quote: chalk(&[ITALIC]),
        quote_border: chalk(&[DIM]),
        hr: chalk(&[DIM]),
        list_bullet: chalk(&[CYAN]),
        bold: chalk(&[BOLD]),
        italic: chalk(&[ITALIC]),
        strikethrough: chalk(&[STRIKETHROUGH]),
        underline: chalk(&[UNDERLINE]),
        highlight_code: None,
        code_block_indent: None,
    }
}

pub fn strip_ansi(line: &str) -> String {
    regex::Regex::new(r"\x1b\[[0-9;]*m").expect("regex").replace_all(line, "").into_owned()
}

/// `markdown.render(width).map(line => stripAnsi(line).trimEnd())`.
pub fn plain(lines: &[String]) -> Vec<String> {
    lines.iter().map(|line| strip_ansi(line).trim_end().to_string()).collect()
}

pub fn markdown(text: &str) -> Markdown {
    Markdown::new(text, 0, 0, default_theme(), None, MarkdownOptions::default())
}

pub fn styled_markdown(text: &str, padding_x: usize, style: DefaultTextStyle) -> Markdown {
    Markdown::new(text, padding_x, 0, default_theme(), Some(style), MarkdownOptions::default())
}

/// Serializes tests that read the process-wide terminal capabilities, and pins them.
pub struct Capabilities {
    _guard: MutexGuard<'static, ()>,
}

impl Capabilities {
    pub fn set(hyperlinks: bool, true_color: bool) -> Self {
        let guard = pi_tui::env::global_state_lock();
        set_capabilities(TerminalCapabilities { images: None, true_color, hyperlinks });
        Self { _guard: guard }
    }

    /// Default for tests that do not care: no hyperlinks.
    pub fn plain() -> Self {
        Self::set(false, false)
    }
}

impl Drop for Capabilities {
    fn drop(&mut self) {
        reset_capabilities_cache();
    }
}

/// A started main-screen TUI showing `component` on a virtual terminal.
pub struct Screen {
    pub terminal: Rc<RefCell<VirtualTerminal>>,
    pub tui: Tui,
}

impl Screen {
    pub fn show(columns: usize, rows: usize, component: ComponentRef) -> Self {
        let terminal = VirtualTerminal::shared(columns, rows);
        let options =
            TuiOptions { env: Some(Rc::new(MapEnv::new(Vec::<(String, String)>::new()))), ..TuiOptions::default() };
        let tui = Tui::new_main_screen(terminal.clone(), options);
        tui.add_child(component);
        tui.start();
        ui_loop::run_for(Duration::from_millis(20)).expect("render");
        Self { terminal, tui }
    }

    pub fn viewport(&self) -> Vec<String> {
        self.terminal.borrow().viewport()
    }

    pub fn cell<T>(&self, row: usize, col: usize, read: impl Fn(&vt100::Cell) -> T) -> T {
        let terminal = self.terminal.borrow();
        let cell = terminal.screen().cell(row as u16, col as u16).unwrap_or_else(|| panic!("no cell {row},{col}"));
        read(cell)
    }

    pub fn fg_default(&self, row: usize, col: usize) -> bool {
        self.cell(row, col, |cell| cell.fgcolor() == vt100::Color::Default)
    }

    pub fn fg(&self, row: usize, col: usize) -> vt100::Color {
        self.cell(row, col, vt100::Cell::fgcolor)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.tui.stop(TuiStopOptions::default());
    }
}

/// Upstream `line.indexOf(text)` in characters.
pub fn column_of(line: &str, needle: &str) -> Option<usize> {
    line.find(needle).map(|index| line[..index].chars().count())
}

/// Upstream `line.indexOf(text, from)` in characters.
pub fn column_from(line: &str, needle: &str, from: usize) -> Option<usize> {
    let start = line.char_indices().nth(from).map_or(line.len(), |(index, _)| index);
    line[start..].find(needle).map(|index| line[..start + index].chars().count())
}

/// Upstream `line.lastIndexOf(text)` in characters.
pub fn last_column_of(line: &str, needle: &str) -> Option<usize> {
    line.rfind(needle).map(|index| line[..index].chars().count())
}
