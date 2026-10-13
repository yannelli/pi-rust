//! A `!` or `!!` shell command with streaming output (upstream `components/bash-execution.ts`).

use std::rc::Rc;

use pi_tui::components::StyleFn;
use pi_tui::{Component, Container, Loader, Spacer, Text, Tui, component};

use super::visual_truncate::{Keep, truncate_to_visual_lines};
use crate::agent_session::bash::strip_ansi;
use crate::interactive_components::{DynamicBorder, key_hint, key_text};
use crate::theme::{ThemeColor, theme};
use crate::tools::truncate::{TruncationOptions, truncate_tail};

/// Lines shown while collapsed.
const PREVIEW_LINES: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Running,
    Complete,
    Cancelled,
    Error,
}

/// Collapsed output lines, re-wrapped when the width changes.
struct PreviewLines {
    text: String,
    padding_x: usize,
    cache: Option<(usize, Vec<String>)>,
}

impl Component for PreviewLines {
    fn render(&mut self, width: usize) -> Vec<String> {
        if let Some((cached, lines)) = &self.cache
            && *cached == width
        {
            return lines.clone();
        }
        let lines = truncate_to_visual_lines(&self.text, PREVIEW_LINES, width, self.padding_x, Keep::End).visual_lines;
        self.cache = Some((width, lines.clone()));
        lines
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }
}

pub struct BashExecutionComponent {
    command: String,
    output_lines: Vec<String>,
    status: Status,
    exit_code: Option<i64>,
    loader: Rc<std::cell::RefCell<Loader>>,
    truncated: bool,
    full_output_path: Option<String>,
    expanded: bool,
    output_pad: usize,
    /// `Dim` for `!!` commands, whose output stays out of the model context.
    color: ThemeColor,
    root: Container,
    content: Rc<std::cell::RefCell<Container>>,
}

impl BashExecutionComponent {
    pub fn new(command: &str, tui: &Tui, exclude_from_context: bool, output_pad: usize) -> Self {
        let color = if exclude_from_context { ThemeColor::Dim } else { ThemeColor::BashMode };
        let border: StyleFn = Rc::new(move |text: &str| theme().fg(color, text));
        let spinner: StyleFn = Rc::new(move |text: &str| theme().fg(color, text));
        let muted: StyleFn = Rc::new(|text: &str| theme().fg(ThemeColor::Muted, text));
        let message = format!("Running... ({} to cancel)", key_text("tui.select.cancel"));
        let loader = Rc::new(std::cell::RefCell::new(Loader::new(Some(tui), spinner, muted, &message, None)));
        let content = component(Container::new());
        let mut root = Container::new();
        root.add_child(component(Spacer::new(1)));
        let border_style = border.clone();
        root.add_child(component(DynamicBorder::with_color(move |text| border_style(text))));
        root.add_child(content.clone());
        root.add_child(component(DynamicBorder::with_color(move |text| border(text))));
        let mut component = Self {
            command: command.to_string(),
            output_lines: Vec::new(),
            status: Status::Running,
            exit_code: None,
            loader,
            truncated: false,
            full_output_path: None,
            expanded: false,
            output_pad,
            color,
            root,
            content,
        };
        component.update_display();
        component
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.update_display();
    }

    pub fn set_output_pad(&mut self, padding: usize) {
        self.output_pad = padding;
        self.update_display();
    }

    /// Append a chunk; ANSI codes are stripped and line endings normalized.
    pub fn append_output(&mut self, chunk: &str) {
        let clean = strip_ansi(chunk).replace("\r\n", "\n").replace('\r', "\n");
        let mut pieces = clean.split('\n');
        match (self.output_lines.last_mut(), pieces.next()) {
            (Some(last), Some(first)) => last.push_str(first),
            (None, Some(first)) => self.output_lines.push(first.to_string()),
            _ => {}
        }
        self.output_lines.extend(pieces.map(str::to_string));
        self.update_display();
    }

    pub fn set_complete(
        &mut self,
        exit_code: Option<i64>,
        cancelled: bool,
        truncated: bool,
        full_output_path: Option<String>,
    ) {
        self.exit_code = exit_code;
        self.status = match (cancelled, exit_code) {
            (true, _) => Status::Cancelled,
            (false, Some(code)) if code != 0 => Status::Error,
            _ => Status::Complete,
        };
        self.truncated = truncated;
        self.full_output_path = full_output_path;
        self.loader.borrow_mut().stop();
        self.update_display();
    }

    pub fn output(&self) -> String {
        self.output_lines.join("\n")
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    fn update_display(&mut self) {
        let theme = theme();
        let full = self.output_lines.join("\n");
        let context = truncate_tail(&full, TruncationOptions::default());
        let available: Vec<&str> =
            if context.content.is_empty() { Vec::new() } else { context.content.split('\n').collect() };
        let preview = &available[available.len().saturating_sub(PREVIEW_LINES)..];
        let hidden = available.len() - preview.len();
        let pad = self.output_pad;

        let mut content = self.content.borrow_mut();
        content.clear();
        let header = theme.fg(self.color, &theme.bold(&format!("$ {}", self.command)));
        content.add_child(component(Text::new(&header, pad, 0)));
        if !available.is_empty() {
            let style = |lines: &[&str]| {
                lines.iter().map(|line| theme.fg(ThemeColor::Muted, line)).collect::<Vec<_>>().join("\n")
            };
            if self.expanded {
                content.add_child(component(Text::new(&format!("\n{}", style(&available)), pad, 0)));
            } else {
                let text = format!("\n{}", style(preview));
                content.add_child(component(PreviewLines { text, padding_x: pad, cache: None }));
            }
        }
        if self.status == Status::Running {
            content.add_child(self.loader.clone());
            return;
        }
        let mut parts = Vec::new();
        if hidden > 0 {
            let muted = |text: &str| theme.fg(ThemeColor::Muted, text);
            if self.expanded {
                parts.push(format!("{}{}{}", muted("("), key_hint("app.tools.expand", "to collapse"), muted(")")));
            } else {
                parts.push(format!(
                    "{}{}{}",
                    muted(&format!("... {hidden} more lines (")),
                    key_hint("app.tools.expand", "to expand"),
                    muted(")")
                ));
            }
        }
        match self.status {
            Status::Cancelled => parts.push(theme.fg(ThemeColor::Warning, "(cancelled)")),
            Status::Error => {
                let code = self.exit_code.map(|code| code.to_string()).unwrap_or_default();
                parts.push(theme.fg(ThemeColor::Error, &format!("(exit {code})")));
            }
            _ => {}
        }
        if (self.truncated || context.truncated)
            && let Some(path) = &self.full_output_path
        {
            parts.push(theme.fg(ThemeColor::Warning, &format!("Output truncated. Full output: {path}")));
        }
        if !parts.is_empty() {
            content.add_child(component(Text::new(&format!("\n{}", parts.join("\n")), pad, 0)));
        }
    }
}

impl Component for BashExecutionComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.root.render(width)
    }

    fn invalidate(&mut self) {
        self.root.invalidate();
        self.update_display();
    }
}
