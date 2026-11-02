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
use crate::extension_ui::ChatStyle;
use crate::markdown_transform::{MarkdownMessageType, MessageMeta, create_markdown_transform};
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

/// The byte offset of visible column `column` in `line`, past escape sequences before it.
fn column_offset(line: &str, column: usize) -> usize {
    let mut seen = 0;
    let mut chars = line.char_indices().peekable();
    while let Some(&(at, ch)) = chars.peek() {
        if ch == '\x1b' {
            chars.next();
            if chars.next_if(|(_, ch)| *ch == '[').is_some() {
                while chars.next_if(|(_, ch)| !('@'..='~').contains(ch)).is_some() {}
            }
            chars.next();
            continue;
        }
        if seen >= column {
            return at;
        }
        seen += pi_tui::visible_width(&ch.to_string());
        chars.next();
    }
    line.len()
}

/// `prefix` at column `pad` of the first line with text, and as many spaces on the others.
fn prefix_lines(lines: Vec<String>, prefix: &str, pad: usize) -> Vec<String> {
    let indent = " ".repeat(pi_tui::visible_width(prefix));
    let first = lines.iter().position(|line| !pi_tui::utils::strip_terminal_sequences(line).trim().is_empty());
    lines
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            let offset = column_offset(&line, pad);
            line.insert_str(offset, if Some(index) == first { prefix } else { &indent });
            line
        })
        .collect()
}

/// Renders `child` narrower by the prefix width and puts the prefix in front.
struct Prefixed {
    child: Markdown,
    prefix: String,
    pad: usize,
}

impl Component for Prefixed {
    fn render(&mut self, width: usize) -> Vec<String> {
        let prefix_width = pi_tui::visible_width(&self.prefix);
        if width <= prefix_width + self.pad + 1 {
            return self.child.render(width);
        }
        prefix_lines(self.child.render(width - prefix_width), &self.prefix, self.pad)
    }

    fn invalidate(&mut self) {
        self.child.invalidate();
    }
}

fn fg_style(color: ThemeColor) -> pi_tui::components::StyleFn {
    Rc::new(move |text: &str| theme().fg(color, text))
}

/// A user message: Markdown on the user message background.
pub struct UserMessageComponent {
    markdown: Markdown,
    pad: usize,
    style: ChatStyle,
}

impl UserMessageComponent {
    pub fn new(text: &str, options: &RenderOptions) -> Self {
        Self::with_meta(text, options, None)
    }

    /// A message replayed from the session entry `meta`, which Markdown transformers receive.
    pub fn with_meta(text: &str, options: &RenderOptions, meta: Option<MessageMeta>) -> Self {
        let style = DefaultTextStyle {
            color: Some(fg_style(ThemeColor::UserMessageText)),
            bg_color: Some(Rc::new(|text: &str| theme().bg(ThemeBg::UserMessageBg, text))),
            ..DefaultTextStyle::default()
        };
        let markdown_options = MarkdownOptions {
            preserve_ordered_list_markers: true,
            preserve_backslash_escapes: true,
            transform: Some(create_markdown_transform(
                MarkdownMessageType::User,
                false,
                options.transformers.clone(),
                meta,
            )),
            ..MarkdownOptions::default()
        };
        let markdown =
            Markdown::new(text, options.output_pad, 1, options.markdown_theme.clone(), Some(style), markdown_options);
        Self { markdown, pad: options.output_pad, style: options.chat_style.clone() }
    }

    pub fn set_chat_style(&mut self, style: ChatStyle) {
        self.style = style;
        self.markdown.invalidate();
    }
}

impl Component for UserMessageComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let prefix = self.style.user_prefix.as_deref().unwrap_or_default();
        let prefix_width = pi_tui::visible_width(prefix);
        if prefix.is_empty() || width <= prefix_width + self.pad + 1 {
            return mark_zone(self.markdown.render(width));
        }
        mark_zone(prefix_lines(self.markdown.render(width - prefix_width), prefix, self.pad))
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
    meta: Option<MessageMeta>,
    /// What `message_display` handlers show in place of the text blocks.
    display_text: Option<String>,
    overrides: HashMap<usize, bool>,
    thinking_runs: usize,
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
        Self::with_meta(message, hide_thinking_block, hidden_thinking_label, options, None)
    }

    /// A message replayed from the session entry `meta`, which Markdown transformers receive.
    pub fn with_meta(
        message: Option<&AssistantMessage>,
        hide_thinking_block: bool,
        hidden_thinking_label: &str,
        options: RenderOptions,
        meta: Option<MessageMeta>,
    ) -> Self {
        let mut component = Self {
            content: Container::new(),
            options,
            hide_thinking_block,
            hidden_thinking_label: hidden_thinking_label.to_string(),
            last_message: None,
            has_tool_calls: false,
            is_streaming: false,
            meta,
            display_text: None,
            overrides: HashMap::new(),
            thinking_runs: 0,
            pending_toggle: Rc::default(),
        };
        if let Some(message) = message {
            component.update_content(message, false);
        }
        component
    }

    /// The thinking toggle; showing also opens the runs that collapsed thinking closed.
    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        self.hide_thinking_block = hide;
        self.overrides.clear();
        if !hide && self.options.chat_style.collapse_thinking {
            self.overrides.extend((0..self.thinking_runs).map(|index| (index, false)));
        }
        self.rebuild();
    }

    pub fn set_hidden_thinking_label(&mut self, label: &str) {
        self.hidden_thinking_label = label.to_string();
        self.rebuild();
    }

    /// Builds the message again with `style`.
    pub fn set_chat_style(&mut self, style: ChatStyle) {
        self.options.chat_style = style;
        self.rebuild();
    }

    pub fn set_output_pad(&mut self, padding: usize) {
        self.options.output_pad = padding;
        self.rebuild();
    }

    /// The message last drawn.
    pub fn message(&self) -> Option<&AssistantMessage> {
        self.last_message.as_ref()
    }

    /// Show `text` in place of the message text; the message itself stays unchanged.
    pub fn set_display_text(&mut self, text: Option<String>) {
        if self.display_text != text {
            self.display_text = text;
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        if let Some(message) = self.last_message.take() {
            let streaming = self.is_streaming;
            self.update_content(&message, streaming);
        }
    }

    fn markdown(&self, text: &str, kind: MarkdownMessageType, style: Option<DefaultTextStyle>) -> Markdown {
        let transformers = self.options.transformers.clone();
        let transform = create_markdown_transform(kind, self.is_streaming, transformers, self.meta.clone());
        let options = MarkdownOptions { transform: Some(transform), ..MarkdownOptions::default() };
        Markdown::new(text, self.options.output_pad, 0, self.options.markdown_theme.clone(), style, options)
    }

    pub fn update_content(&mut self, message: &AssistantMessage, is_streaming: bool) {
        self.is_streaming = is_streaming;
        self.content.clear();
        // Spacers go in right before the next visible block: one before the first, and one after
        // each visible thinking run that more visible content follows (upstream #11402).
        let mut needs_spacer = true;
        let add_visible = |content: &mut Container, needs_spacer: &mut bool, child: pi_tui::ComponentRef| {
            if std::mem::take(needs_spacer) {
                content.add_child(component(Spacer::new(1)));
            }
            content.add_child(child);
        };
        let pad = self.options.output_pad;
        let style = self.options.chat_style.clone();
        let mut prefix = style.assistant_prefix.clone();
        let mut run_index = 0;
        let mut i = 0;
        // The display text takes the place of the first text block and hides the others.
        let mut display = self.display_text.clone();
        while i < message.content.len() {
            match &message.content[i] {
                ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                    let shown = match (&self.display_text, display.take()) {
                        (None, _) => Some(text.trim().to_string()),
                        (Some(_), replacement) => replacement.filter(|text| !text.trim().is_empty()),
                    };
                    if let Some(shown) = shown {
                        let markdown = self.markdown(shown.trim(), MarkdownMessageType::Assistant, None);
                        let child: pi_tui::ComponentRef = match prefix.take() {
                            Some(prefix) => component(Prefixed { child: markdown, prefix, pad }),
                            None => component(markdown),
                        };
                        add_visible(&mut self.content, &mut needs_spacer, child);
                    }
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
                    let index = run_index;
                    run_index += 1;
                    // With collapsed thinking only the run being written stays open.
                    let writing = is_streaming && i >= message.content.len();
                    let collapsed = self.hide_thinking_block || (style.collapse_thinking && !writing);
                    let hidden = self.overrides.get(&index).copied().unwrap_or(collapsed);
                    // A hidden run with an empty label renders nothing, not even spacing.
                    if hidden && self.hidden_thinking_label.trim().is_empty() {
                        continue;
                    }
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
                    add_visible(&mut self.content, &mut needs_spacer, component(region));
                    needs_spacer = true;
                }
                _ => i += 1,
            }
        }

        self.thinking_runs = run_index;
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use pi_tui::utils::strip_terminal_sequences;

    use super::*;
    use crate::markdown_transform::MarkdownTransformer;

    type Captured = Arc<Mutex<Vec<(MarkdownMessageType, Option<String>, Option<String>)>>>;

    fn capturing_options() -> (RenderOptions, Captured) {
        let captured = Captured::default();
        let sink = captured.clone();
        let capture: MarkdownTransformer = Arc::new(move |_, context| {
            let seen = (context.message_type, context.message_id.clone(), context.timestamp.clone());
            sink.lock().unwrap().push(seen);
            None
        });
        (RenderOptions { transformers: vec![capture], ..RenderOptions::default() }, captured)
    }

    fn meta() -> Option<MessageMeta> {
        Some(MessageMeta { id: "entry-abc".into(), timestamp: "2025-01-15T10:30:00.000Z".into() })
    }

    #[test]
    fn passes_persisted_entry_identity_to_transformers_and_leaves_live_messages_without() {
        let (options, captured) = capturing_options();
        UserMessageComponent::with_meta("restored", &options, meta()).render(80);
        UserMessageComponent::new("live", &options).render(80);
        let restored = (Some("entry-abc".to_string()), Some("2025-01-15T10:30:00.000Z".to_string()));
        assert_eq!(
            *captured.lock().unwrap(),
            [
                (MarkdownMessageType::User, restored.0.clone(), restored.1.clone()),
                (MarkdownMessageType::User, None, None)
            ]
        );

        let (options, captured) = capturing_options();
        let message = AssistantMessage {
            content: vec![
                ContentBlock::Thinking { thinking: "hmm".into(), thinking_signature: None, redacted: false },
                ContentBlock::Text { text: "answer".into(), text_signature: None },
            ],
            ..AssistantMessage::default()
        };
        AssistantMessageComponent::with_meta(Some(&message), false, "Thinking...", options, meta()).render(80);
        let kinds = [MarkdownMessageType::AssistantThinking, MarkdownMessageType::Assistant];
        let expected: Vec<_> = kinds.into_iter().map(|kind| (kind, restored.0.clone(), restored.1.clone())).collect();
        assert_eq!(*captured.lock().unwrap(), expected);
    }

    fn thinking_then(next: ContentBlock, stop_reason: StopReason) -> AssistantMessage {
        let thinking =
            ContentBlock::Thinking { thinking: "reasoning".into(), thinking_signature: None, redacted: false };
        AssistantMessage { content: vec![thinking, next], stop_reason, ..AssistantMessage::default() }
    }

    fn rows(message: &AssistantMessage, label: &str) -> Vec<String> {
        let mut component = AssistantMessageComponent::new(Some(message), true, label, RenderOptions::default());
        component.render(80).iter().map(|line| strip_terminal_sequences(line).trim().to_string()).collect()
    }

    // Upstream #11402: hidden thinking with an empty label drew blank rows.
    #[test]
    fn hidden_thinking_with_an_empty_label_renders_no_rows() {
        let call = pi_ai::ToolCall {
            id: "tool-1".into(),
            name: "read".into(),
            arguments: serde_json::from_value(serde_json::json!({"path": "file.txt"})).unwrap(),
            ..pi_ai::ToolCall::default()
        };
        let tool = thinking_then(ContentBlock::ToolCall(call), StopReason::ToolUse);
        assert!(rows(&tool, "").is_empty());
        let text = thinking_then(ContentBlock::text("answer"), StopReason::Stop);
        assert_eq!(rows(&text, ""), ["", "answer"]);
        assert_eq!(rows(&text, "Thinking..."), ["", "Thinking...", "", "answer"]);
    }

    #[test]
    fn display_text_replaces_the_text_blocks_and_keeps_the_message() {
        let mut message = thinking_then(ContentBlock::text("first"), StopReason::Stop);
        message.content.push(ContentBlock::text("second"));
        let mut component =
            AssistantMessageComponent::new(Some(&message), true, "Thinking...", RenderOptions::default());
        component.set_display_text(Some("shown".into()));
        let drawn: Vec<String> =
            component.render(80).iter().map(|line| strip_terminal_sequences(line).trim().to_string()).collect();
        assert_eq!(drawn, ["", "Thinking...", "", "shown"]);
        assert_eq!(component.message(), Some(&message));
    }

    fn styled() -> RenderOptions {
        let chat_style = ChatStyle {
            user_prefix: Some("› ".into()),
            assistant_prefix: Some("● ".into()),
            collapse_thinking: true,
            rounded_editor: false,
        };
        RenderOptions { chat_style, ..RenderOptions::default() }
    }

    fn plain(lines: Vec<String>) -> Vec<String> {
        lines.iter().map(|line| strip_terminal_sequences(line).trim_end().to_string()).collect()
    }

    #[test]
    fn chat_style_prefixes_the_first_line_and_indents_the_rest() {
        let mut user = UserMessageComponent::new("hello\n\nthere", &styled());
        let lines = user.render(30);
        assert!(lines.iter().all(|line| pi_tui::visible_width(line) == 30), "{lines:?}");
        assert_eq!(plain(lines), ["", " › hello", "", "   there", ""]);
        user.set_chat_style(ChatStyle::default());
        assert_eq!(plain(user.render(30))[1], " hello");

        let text = thinking_then(ContentBlock::text("answer"), StopReason::Stop);
        let mut done = AssistantMessageComponent::new(Some(&text), false, "∴ Thought", styled());
        assert_eq!(plain(done.render(30)), ["", " ∴ Thought", "", " ● answer"]);
        done.set_chat_style(ChatStyle::default());
        assert_eq!(plain(done.render(30)), ["", " reasoning", "", " answer"]);
    }

    #[test]
    fn collapsed_thinking_keeps_the_run_being_written_open() {
        let thinking =
            ContentBlock::Thinking { thinking: "planning".into(), thinking_signature: None, redacted: false };
        let partial = AssistantMessage { content: vec![thinking], ..AssistantMessage::default() };
        let mut streaming = AssistantMessageComponent::new(None, false, "∴ Thought", styled());
        streaming.update_content(&partial, true);
        assert_eq!(plain(streaming.render(30)), ["", " planning"]);
        streaming.update_content(&partial, false);
        assert_eq!(plain(streaming.render(30)), ["", " ∴ Thought"]);
        streaming.set_hide_thinking_block(true);
        streaming.set_hide_thinking_block(false);
        assert_eq!(plain(streaming.render(30)), ["", " planning"], "the toggle opens collapsed runs");
    }
}
