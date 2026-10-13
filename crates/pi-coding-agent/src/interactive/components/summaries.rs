//! Collapsible boxes for compaction summaries, branch summaries, and skill invocations, and the
//! default rendering of extension custom messages (upstream `compaction-summary-message.ts`,
//! `branch-summary-message.ts`, `skill-invocation-message.ts`, and `custom-message.ts`).

use std::cell::Cell;
use std::rc::Rc;

use pi_ai::{ContentBlock, UserContent};
use pi_tui::components::markdown::{DefaultTextStyle, MarkdownOptions};
use pi_tui::{
    BoxComponent, Component, ComponentRef, Container, Markdown, MouseRegion, MouseResponse, Spacer, Text,
    TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType, component,
};

use super::RenderOptions;
use crate::interactive_components::key_text;
use crate::messages::CustomMessage;
use crate::resources::ParsedSkillBlock;
use crate::theme::{ThemeBg, ThemeColor, theme};

/// `1234567` as `1,234,567` (upstream `toLocaleString()` in en-US).
pub fn format_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn custom_bg() -> pi_tui::components::StyleFn {
    Rc::new(|text: &str| theme().bg(ThemeBg::CustomMessageBg, text))
}

fn label(name: &str) -> String {
    theme().fg(ThemeColor::CustomMessageLabel, &format!("\x1b[1m[{name}]\x1b[22m"))
}

fn message_markdown(text: &str, options: &RenderOptions) -> ComponentRef {
    let color: pi_tui::components::StyleFn = Rc::new(|text: &str| theme().fg(ThemeColor::CustomMessageText, text));
    let style = DefaultTextStyle { color: Some(color), ..DefaultTextStyle::default() };
    component(Markdown::new(text, 0, 0, options.markdown_theme.clone(), Some(style), MarkdownOptions::default()))
}

/// What a collapsible box shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Collapsible {
    Compaction { summary: String, tokens_before: u64 },
    BranchSummary { summary: String },
    Skill(ParsedSkillBlock),
}

/// A box on the custom message background that expands on `Ctrl+O` or a click.
pub struct CollapsibleMessage {
    kind: Collapsible,
    options: RenderOptions,
    expanded: bool,
    container: BoxComponent,
    clicked: Rc<Cell<bool>>,
}

impl CollapsibleMessage {
    pub fn new(kind: Collapsible, options: RenderOptions) -> Self {
        let container = BoxComponent::new(options.output_pad, 1, Some(custom_bg()));
        let mut message = Self { kind, options, expanded: false, container, clicked: Rc::default() };
        message.update_display();
        message
    }

    pub fn kind(&self) -> &Collapsible {
        &self.kind
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.update_display();
    }

    pub fn set_output_pad(&mut self, padding: usize) {
        self.container.set_padding_x(padding);
    }

    fn collapsed_hint(text: &str) -> String {
        let theme = theme();
        theme.fg(ThemeColor::CustomMessageText, &format!("{text} ("))
            + &theme.fg(ThemeColor::Dim, &key_text("app.tools.expand"))
            + &theme.fg(ThemeColor::CustomMessageText, " to expand)")
    }

    fn update_display(&mut self) {
        self.container.clear();
        let mut content = Container::new();
        match &self.kind {
            Collapsible::Compaction { summary, tokens_before } => {
                let tokens = format_thousands(*tokens_before);
                content.add_child(component(Text::new(&label("compaction"), 0, 0)));
                content.add_child(component(Spacer::new(1)));
                if self.expanded {
                    let text = format!("**Compacted from {tokens} tokens**\n\n{summary}");
                    content.add_child(message_markdown(&text, &self.options));
                } else {
                    let hint = Self::collapsed_hint(&format!("Compacted from {tokens} tokens"));
                    content.add_child(component(Text::new(&hint, 0, 0)));
                }
            }
            Collapsible::BranchSummary { summary } => {
                content.add_child(component(Text::new(&label("branch"), 0, 0)));
                content.add_child(component(Spacer::new(1)));
                if self.expanded {
                    content.add_child(message_markdown(&format!("**Branch Summary**\n\n{summary}"), &self.options));
                } else {
                    content.add_child(component(Text::new(&Self::collapsed_hint("Branch summary"), 0, 0)));
                }
            }
            Collapsible::Skill(block) => {
                let theme = theme();
                if self.expanded {
                    content.add_child(component(Text::new(&label("skill"), 0, 0)));
                    let text = format!("**{}**\n\n{}", block.name, block.content);
                    content.add_child(message_markdown(&text, &self.options));
                } else {
                    let line = theme.fg(ThemeColor::CustomMessageLabel, "\x1b[1m[skill]\x1b[22m ")
                        + &theme.fg(ThemeColor::CustomMessageText, &block.name)
                        + &theme.fg(ThemeColor::Dim, &format!(" ({} to expand)", key_text("app.tools.expand")));
                    content.add_child(component(Text::new(&line, 0, 0)));
                }
            }
        }
        let clicked = self.clicked.clone();
        let region = MouseRegion::new(component(content), move |event| {
            if event.kind != TuiMouseEventType::Click || event.button != TuiMouseButton::Left {
                return None;
            }
            clicked.set(true);
            Some(TuiMouseEventResult::handled())
        });
        self.container.add_child(component(region));
    }
}

impl Component for CollapsibleMessage {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let response = self.container.handle_mouse(event);
        if self.clicked.take() {
            self.set_expanded(!self.expanded);
        }
        response
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
        self.update_display();
    }
}

/// The text of a user-style content value.
pub fn user_content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n"),
    }
}

/// An extension message with `display: true`, drawn as a labeled box.
pub struct CustomMessageComponent {
    message: CustomMessage,
    options: RenderOptions,
    container: Container,
    expanded: bool,
}

impl CustomMessageComponent {
    pub fn new(message: CustomMessage, options: RenderOptions) -> Self {
        let mut component = Self { message, options, container: Container::new(), expanded: false };
        component.rebuild();
        component
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        if self.expanded != expanded {
            self.expanded = expanded;
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        self.container.clear();
        self.container.add_child(component(Spacer::new(1)));
        let mut body = BoxComponent::new(self.options.output_pad, 1, Some(custom_bg()));
        body.add_child(component(Text::new(&label(&self.message.custom_type), 0, 0)));
        body.add_child(component(Spacer::new(1)));
        body.add_child(message_markdown(&user_content_text(&self.message.content), &self.options));
        self.container.add_child(component(body));
    }
}

impl Component for CustomMessageComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn invalidate(&mut self) {
        self.rebuild();
    }
}

#[cfg(test)]
mod tests {
    use super::format_thousands;

    #[test]
    fn formats_thousands_like_en_us() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1000), "1,000");
        assert_eq!(format_thousands(1234567), "1,234,567");
    }
}
