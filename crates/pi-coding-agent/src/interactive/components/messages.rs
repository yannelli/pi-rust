//! Chat message components: user messages and assistant messages with collapsible thinking
//! (upstream `components/user-message.ts` and `components/assistant-message.ts`).

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use pi_ai::{AssistantMessage, ContentBlock, StopReason};
use pi_tui::components::markdown::{DefaultTextStyle, MarkdownOptions};
use pi_tui::{
    Component, Container, Markdown, MouseRegion, MouseResponse, Spacer, Text, TuiMouseButton, TuiMouseEvent,
    TuiMouseEventResult, TuiMouseEventType, component,
};

use super::RenderOptions;
use crate::markdown_transform::{MarkdownMessageType, create_markdown_transform};
use crate::theme::{ThemeBg, ThemeColor, theme};

const OSC133_ZONE_START: &str = "\x1b]133;A\x07";
const OSC133_ZONE_END: &str = "\x1b]133;B\x07";
const OSC133_ZONE_FINAL: &str = "\x1b]133;C\x07";

/// Mark the lines as one prompt zone for terminals that support OSC 133 jumps.
fn mark_zone(mut lines: Vec<String>) -> Vec<String> {
    if let Some(first) = lines.first_mut() {
        first.insert_str(0, OSC133_ZONE_START);
    }
    if let Some(last) = lines.last_mut() {
        last.insert_str(0, &format!("{OSC133_ZONE_END}{OSC133_ZONE_FINAL}"));
    }
    lines
}

fn fg_style(color: ThemeColor) -> pi_tui::components::StyleFn {
    Rc::new(move |text: &str| theme().fg(color, text))
}

/// A user message: Markdown on the user message background.
pub struct UserMessageComponent {
    markdown: Markdown,
}

impl UserMessageComponent {
    pub fn new(text: &str, options: &RenderOptions) -> Self {
        let style = DefaultTextStyle {
            color: Some(fg_style(ThemeColor::UserMessageText)),
            bg_color: Some(Rc::new(|text: &str| theme().bg(ThemeBg::UserMessageBg, text))),
            ..DefaultTextStyle::default()
        };
        let markdown_options = MarkdownOptions {
            preserve_ordered_list_markers: true,
            preserve_backslash_escapes: true,
            transform: Some(create_markdown_transform(MarkdownMessageType::User, false, options.transformers.clone())),
            ..MarkdownOptions::default()
        };
        let markdown =
            Markdown::new(text, options.output_pad, 1, options.markdown_theme.clone(), Some(style), markdown_options);
        Self { markdown }
    }
}

impl Component for UserMessageComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        mark_zone(self.markdown.render(width))
    }

    fn invalidate(&mut self) {
        self.markdown.invalidate();
    }
}

/// A complete or streaming assistant message. Clicking a thinking block toggles it.
pub struct AssistantMessageComponent {
    content: Container,
    options: RenderOptions,
    hide_thinking_block: bool,
    hidden_thinking_label: String,
    last_message: Option<AssistantMessage>,
    has_tool_calls: bool,
    is_streaming: bool,
    overrides: HashMap<usize, bool>,
    /// A thinking run clicked during mouse dispatch, applied once dispatch returns.
    pending_toggle: Rc<Cell<Option<(usize, bool)>>>,
}

impl AssistantMessageComponent {
    pub fn new(
        message: Option<&AssistantMessage>,
        hide_thinking_block: bool,
        hidden_thinking_label: &str,
        options: RenderOptions,
    ) -> Self {
        let mut component = Self {
            content: Container::new(),
            options,
            hide_thinking_block,
            hidden_thinking_label: hidden_thinking_label.to_string(),
            last_message: None,
            has_tool_calls: false,
            is_streaming: false,
            overrides: HashMap::new(),
            pending_toggle: Rc::default(),
        };
        if let Some(message) = message {
            component.update_content(message, false);
        }
        component
    }

    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        self.hide_thinking_block = hide;
        self.overrides.clear();
        self.rebuild();
    }

    pub fn set_hidden_thinking_label(&mut self, label: &str) {
        self.hidden_thinking_label = label.to_string();
        self.rebuild();
    }

    pub fn set_output_pad(&mut self, padding: usize) {
        self.options.output_pad = padding;
        self.rebuild();
    }

    fn rebuild(&mut self) {
        if let Some(message) = self.last_message.take() {
            let streaming = self.is_streaming;
            self.update_content(&message, streaming);
        }
    }

    fn markdown(&self, text: &str, kind: MarkdownMessageType, style: Option<DefaultTextStyle>) -> Markdown {
        let transform = create_markdown_transform(kind, self.is_streaming, self.options.transformers.clone());
        let options = MarkdownOptions { transform: Some(transform), ..MarkdownOptions::default() };
        Markdown::new(text, self.options.output_pad, 0, self.options.markdown_theme.clone(), style, options)
    }

    pub fn update_content(&mut self, message: &AssistantMessage, is_streaming: bool) {
        self.is_streaming = is_streaming;
        self.content.clear();
        let visible = |block: &ContentBlock| match block {
            ContentBlock::Text { text, .. } => !text.trim().is_empty(),
            ContentBlock::Thinking { thinking, .. } => !thinking.trim().is_empty(),
            _ => false,
        };
        if message.content.iter().any(visible) {
            self.content.add_child(component(Spacer::new(1)));
        }
        let pad = self.options.output_pad;
        let mut run_index = 0;
        let mut i = 0;
        while i < message.content.len() {
            match &message.content[i] {
                ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                    let markdown = self.markdown(text.trim(), MarkdownMessageType::Assistant, None);
                    self.content.add_child(component(markdown));
                    i += 1;
                }
                ContentBlock::Thinking { .. } => {
                    let mut blocks = Vec::new();
                    while let Some(ContentBlock::Thinking { thinking, .. }) = message.content.get(i) {
                        if !thinking.trim().is_empty() {
                            blocks.push(thinking.trim().to_string());
                        }
                        i += 1;
                    }
                    if blocks.is_empty() {
                        continue;
                    }
                    let visible_after = message.content[i..].iter().any(visible);
                    let index = run_index;
                    run_index += 1;
                    let hidden = self.overrides.get(&index).copied().unwrap_or(self.hide_thinking_block);
                    let body: pi_tui::ComponentRef = if hidden {
                        let label = theme().italic(&theme().fg(ThemeColor::ThinkingText, &self.hidden_thinking_label));
                        component(Text::new(&label, pad, 0))
                    } else {
                        let style = DefaultTextStyle {
                            color: Some(fg_style(ThemeColor::ThinkingText)),
                            italic: true,
                            ..DefaultTextStyle::default()
                        };
                        component(self.markdown(
                            &blocks.join("\n\n"),
                            MarkdownMessageType::AssistantThinking,
                            Some(style),
                        ))
                    };
                    let pending = self.pending_toggle.clone();
                    let region = MouseRegion::new(body, move |event| {
                        if event.kind != TuiMouseEventType::Click || event.button != TuiMouseButton::Left {
                            return None;
                        }
                        pending.set(Some((index, !hidden)));
                        Some(TuiMouseEventResult::handled())
                    });
                    self.content.add_child(component(region));
                    if visible_after {
                        self.content.add_child(component(Spacer::new(1)));
                    }
                }
                _ => i += 1,
            }
        }

        self.has_tool_calls = message.content.iter().any(|block| matches!(block, ContentBlock::ToolCall(_)));
        let error_line = |text: &str| component(Text::new(&theme().fg(ThemeColor::Error, text), pad, 0));
        if message.stop_reason == StopReason::Length {
            self.content.add_child(component(Spacer::new(1)));
            self.content.add_child(error_line("Response was truncated before completion."));
        } else if !self.has_tool_calls {
            let error = message.error_message.as_deref();
            let text = match message.stop_reason {
                StopReason::Aborted => Some(match error.filter(|e| *e != "Request was aborted") {
                    Some(error) => error.to_string(),
                    None => "Operation aborted".to_string(),
                }),
                StopReason::Error => Some(format!("Error: {}", error.unwrap_or("Unknown error"))),
                _ => None,
            };
            if let Some(text) = text {
                self.content.add_child(component(Spacer::new(1)));
                self.content.add_child(error_line(&text));
            }
        }
        self.last_message = Some(message.clone());
    }
}

impl Component for AssistantMessageComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let lines = self.content.render(width);
        if self.has_tool_calls || lines.is_empty() { lines } else { mark_zone(lines) }
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let response = self.content.handle_mouse(event);
        if let Some((index, hidden)) = self.pending_toggle.take() {
            self.overrides.insert(index, hidden);
            self.rebuild();
        }
        response
    }

    fn invalidate(&mut self) {
        self.content.invalidate();
        self.rebuild();
    }

    fn container_children(&self) -> Option<Vec<pi_tui::ComponentRef>> {
        Some(self.content.children.clone())
    }
}
