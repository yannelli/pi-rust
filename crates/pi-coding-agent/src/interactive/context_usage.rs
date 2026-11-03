//! `/context` (pi-rust extension): how much of the model's context window the conversation
//! uses, with an estimated breakdown of the next request by part.

use pi_ai::{JsonObject, Message, SystemMessage, Tool, UserContent};

use super::mode::InteractiveMode;
use super::util::format_tokens;
use crate::compaction::{ContextUsageEstimate, estimate_projected_context_tokens, estimate_tokens};
use crate::system_prompt::{SystemPromptState, build_system_prompt_state, sections_to_json};
use crate::theme::{ThemeColor, theme};

const GRID_COLUMNS: usize = 20;
const GRID_ROWS: usize = 5;
const GRID_DOTS: usize = GRID_COLUMNS * GRID_ROWS;
const USED_DOT: &str = "●";
const FREE_DOT: &str = "○";
const LABEL_WIDTH: usize = 15;
const ROW_WIDTH: usize = 20;

/// Estimated tokens of each part of the next request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ContextBreakdown {
    system_prompt: u64,
    tool_definitions: u64,
    context_files: u64,
    skills: u64,
    /// User prompts, plus shell command output, extension messages, and summaries.
    user: u64,
    assistant: u64,
    tool_results: u64,
}

impl ContextBreakdown {
    fn rows(&self) -> [(&'static str, u64); 7] {
        [
            ("System prompt", self.system_prompt),
            ("Tool definitions", self.tool_definitions),
            ("Context files", self.context_files),
            ("Skills", self.skills),
            ("User messages", self.user),
            ("Assistant messages", self.assistant),
            ("Tool results", self.tool_results),
        ]
    }

    fn total(&self) -> u64 {
        self.rows().iter().map(|(_, tokens)| tokens).sum()
    }
}

/// The compaction estimate of a system message with these parts.
fn system_tokens(content: &str, sections: Option<JsonObject>, tools: &[Tool]) -> u64 {
    let system = SystemMessage {
        content: UserContent::Text(content.to_string()),
        sections,
        tools_added: (!tools.is_empty()).then(|| tools.to_vec()),
        tools_removed: None,
        timestamp: 0,
    };
    estimate_tokens(&Message::System(system))
}

/// Context files and skills are prompt sections; the rest of the prompt is the system prompt.
/// System messages in `messages` only patch the prompt, so they are not counted again.
fn context_breakdown(prompt: &SystemPromptState, tools: &[Tool], messages: &[Message]) -> ContextBreakdown {
    let sections = prompt.sections.as_deref().unwrap_or_default();
    let section = |name: &str| {
        sections.iter().find(|(section, _)| section == name).map_or(0, |(_, text)| system_tokens(text, None, &[]))
    };
    let (context_files, skills) = (section("project_context"), section("skills"));
    let whole = system_tokens(&prompt.content, prompt.sections.as_ref().map(sections_to_json), &[]);
    let mut breakdown = ContextBreakdown {
        system_prompt: whole.saturating_sub(context_files + skills),
        tool_definitions: system_tokens("", None, tools),
        context_files,
        skills,
        ..ContextBreakdown::default()
    };
    for message in messages {
        let slot = match message {
            Message::System(_) => continue,
            Message::User(_) | Message::Custom(_) => &mut breakdown.user,
            Message::Assistant(_) => &mut breakdown.assistant,
            Message::ToolResult(_) => &mut breakdown.tool_results,
        };
        *slot += estimate_tokens(message);
    }
    breakdown
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UsageSource {
    Response,
    ResponseAndEstimate,
    Estimate,
}

/// The footer's count when a response reported usage, else the estimate of the next request.
fn tokens_in_use(estimate: &ContextUsageEstimate, breakdown: &ContextBreakdown) -> (u64, UsageSource) {
    match (estimate.usage_tokens, estimate.trailing_tokens) {
        (0, _) => (breakdown.total(), UsageSource::Estimate),
        (_, 0) => (estimate.tokens, UsageSource::Response),
        _ => (estimate.tokens, UsageSource::ResponseAndEstimate),
    }
}

struct ContextReport {
    model: Option<String>,
    /// Zero when the model's window is unknown.
    window: u64,
    in_use: u64,
    source: UsageSource,
    /// Where auto-compaction starts; `None` when it is off.
    compact_at: Option<u64>,
    breakdown: ContextBreakdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    Title,
    Label,
    /// A part of the breakdown, by its index in [`ContextBreakdown::rows`].
    Part(usize),
    Free,
}

fn percent(tokens: u64, window: u64) -> String {
    format!("{:.1}%", tokens as f64 / window as f64 * 100.0)
}

/// The part that fills each dot of the grid, or `None` for a free dot. One dot is
/// `1 / GRID_DOTS` of the window; the parts share the used dots in proportion to their size.
fn dot_parts(in_use: u64, window: u64, breakdown: &ContextBreakdown) -> Vec<Option<usize>> {
    let used = (in_use.saturating_mul(GRID_DOTS as u64).div_ceil(window) as usize).min(GRID_DOTS);
    let total = breakdown.total().max(1);
    let mut ends = Vec::new();
    let mut sum = 0;
    for (_, tokens) in breakdown.rows() {
        sum += tokens;
        ends.push(((sum as u128 * used as u128 + total as u128 / 2) / total as u128) as usize);
    }
    let last = ends.len() - 1;
    (0..GRID_DOTS).map(|dot| (dot < used).then(|| ends.iter().position(|&end| end > dot).unwrap_or(last))).collect()
}

fn usage_grid(
    in_use: u64,
    window: u64,
    breakdown: &ContextBreakdown,
    style: &dyn Fn(Style, &str) -> String,
) -> Vec<String> {
    let dots: Vec<String> = dot_parts(in_use, window, breakdown)
        .into_iter()
        .map(|part| match part {
            Some(part) => style(Style::Part(part), USED_DOT),
            None => style(Style::Free, FREE_DOT),
        })
        .collect();
    dots.chunks(GRID_COLUMNS).map(|row| row.join(" ")).collect()
}

fn report_text(report: &ContextReport, style: &dyn Fn(Style, &str) -> String) -> String {
    let window = report.window;
    let mut lines = vec![style(Style::Title, "Context Usage"), String::new()];
    let field =
        |label: &str, value: &str| format!("{} {value}", style(Style::Label, &format!("{label:<LABEL_WIDTH$}")));
    lines.push(field("Model:", report.model.as_deref().unwrap_or("none selected")));
    let shown_window = if window > 0 { format!("{} tokens", format_tokens(window)) } else { "unknown".into() };
    lines.push(field("Window:", &shown_window));
    let source = match report.source {
        UsageSource::Response => "from the last response",
        UsageSource::ResponseAndEstimate => "last response plus estimated new messages",
        UsageSource::Estimate => "estimated",
    };
    let share = if window > 0 { format!(" ({})", percent(report.in_use, window)) } else { String::new() };
    lines.push(field("In use:", &format!("{} tokens{share}, {source}", format_tokens(report.in_use))));
    if window > 0 {
        let compaction = match report.compact_at {
            Some(at) => format!("at {} tokens ({})", format_tokens(at), percent(at, window)),
            None => "off".into(),
        };
        lines.push(field("Auto-compact:", &compaction));
        lines.push(String::new());
        lines.extend(usage_grid(report.in_use, window, &report.breakdown, style));
    }
    lines.push(String::new());
    lines.push(style(Style::Title, "Estimated breakdown"));
    let free = report.window.saturating_sub(report.in_use);
    let free_row = (window > 0).then_some(("Free space", free));
    for (index, (name, tokens)) in report.breakdown.rows().into_iter().chain(free_row).enumerate() {
        let share = if window > 0 { format!(" {:>6}", percent(tokens, window)) } else { String::new() };
        let key = if name == "Free space" { style(Style::Free, FREE_DOT) } else { style(Style::Part(index), USED_DOT) };
        lines.push(format!("  {key} {name:<ROW_WIDTH$}{:>7}{share}", format_tokens(tokens)));
    }
    lines.join("\n")
}

/// The dot color of each part, in the order of [`ContextBreakdown::rows`].
const PART_COLORS: [ThemeColor; 7] = [
    ThemeColor::SyntaxKeyword,
    ThemeColor::SyntaxFunction,
    ThemeColor::SyntaxType,
    ThemeColor::SyntaxNumber,
    ThemeColor::Accent,
    ThemeColor::SyntaxString,
    ThemeColor::Warning,
];

impl InteractiveMode {
    /// `/context`: the report is built once and painted with the current theme.
    pub(super) fn handle_context_command(&self) {
        let session = self.session();
        let selected = session.model();
        let limits = session.limits_model();
        let prompt = build_system_prompt_state(&session.effective_prompt_options())
            .unwrap_or(SystemPromptState { content: String::new(), sections: None });
        let hidden = session.hidden_declarations();
        let tools: Vec<Tool> = session.with_tool_registry(|registry| {
            let declared = registry.active_tools().iter().filter(|tool| !hidden.iter().any(|name| name == tool.name()));
            declared.map(|tool| tool.declaration()).collect()
        });
        let breakdown = context_breakdown(&prompt, &tools, &session.messages());
        let estimate = {
            let manager = session.session_manager();
            estimate_projected_context_tokens(&manager.build_session_projection(), &manager.get_branch(None))
        };
        let (in_use, source) = tokens_in_use(&estimate, &breakdown);
        let window = limits.as_ref().map_or(0, |model| model.context_window);
        let settings = session.compaction_settings(selected.as_ref());
        let compact_at = settings.enabled.then(|| window.saturating_sub(settings.reserve_tokens));
        let model = limits.as_ref().map(|limits| match &selected {
            Some(chosen) if (&chosen.provider, &chosen.id) != (&limits.provider, &limits.id) => {
                format!("{}/{} (routed to {}/{})", chosen.provider, chosen.id, limits.provider, limits.id)
            }
            _ => format!("{}/{}", limits.provider, limits.id),
        });
        let report = ContextReport { model, window, in_use, source, compact_at, breakdown };
        let render = move || {
            let t = theme();
            let style = |style: Style, text: &str| match style {
                Style::Title => t.bold(text),
                Style::Label => t.fg(ThemeColor::Dim, text),
                Style::Part(part) => t.fg(PART_COLORS[part.min(PART_COLORS.len() - 1)], text),
                Style::Free => t.fg(ThemeColor::Dim, text),
            };
            report_text(&report, &style)
        };
        self.add_spacer();
        self.add_text(render, 1, 0);
        self.request_render();
    }
}

#[cfg(test)]
mod tests {
    use pi_ai::{AssistantMessage, ContentBlock, ToolResultMessage, UserMessage};
    use serde_json::json;

    use super::*;

    fn plain(_: Style, text: &str) -> String {
        text.to_string()
    }

    fn text_of(chars: usize) -> String {
        "x".repeat(chars)
    }

    fn sectioned(sections: &[(&str, usize)]) -> SystemPromptState {
        let sections = sections.iter().map(|(name, chars)| (name.to_string(), text_of(*chars))).collect();
        SystemPromptState { content: String::new(), sections: Some(sections) }
    }

    fn system(chars: usize) -> Message {
        let content = UserContent::Text(text_of(chars));
        Message::System(SystemMessage { content, sections: None, tools_added: None, tools_removed: None, timestamp: 0 })
    }

    fn assistant(chars: usize) -> Message {
        let content = vec![ContentBlock::text(text_of(chars))];
        Message::Assistant(AssistantMessage { content, ..AssistantMessage::default() })
    }

    fn tool_result(chars: usize) -> Message {
        let content = vec![ContentBlock::text(text_of(chars))];
        Message::ToolResult(ToolResultMessage { content, ..ToolResultMessage::default() })
    }

    #[test]
    fn splits_the_prompt_sections_tools_and_messages() {
        let prompt = sectioned(&[("preamble", 400), ("project_context", 80), ("skills", 40), ("cwd", 4)]);
        let tools = [Tool::new("read", "Read a file", json!({"type": "object"}))];
        let messages = vec![
            system(800),
            Message::User(UserMessage { content: UserContent::Text(text_of(12)), timestamp: 0 }),
            assistant(20),
            tool_result(400),
        ];
        let breakdown = context_breakdown(&prompt, &tools, &messages);
        let tool_tokens = system_tokens("", None, &tools);
        assert!(tool_tokens > 0);
        let expected = ContextBreakdown {
            system_prompt: 101,
            tool_definitions: tool_tokens,
            context_files: 20,
            skills: 10,
            user: 3,
            assistant: 5,
            tool_results: 100,
        };
        assert_eq!(breakdown, expected);
        assert_eq!(breakdown.total(), 239 + tool_tokens);
    }

    #[test]
    fn a_forced_prompt_counts_as_the_system_prompt() {
        let prompt = SystemPromptState { content: text_of(40), sections: None };
        let breakdown = context_breakdown(&prompt, &[], &[]);
        assert_eq!(breakdown, ContextBreakdown { system_prompt: 10, ..ContextBreakdown::default() });
    }

    #[test]
    fn uses_reported_usage_when_a_response_has_it() {
        let breakdown = ContextBreakdown { system_prompt: 300, ..ContextBreakdown::default() };
        let none = ContextUsageEstimate { tokens: 0, usage_tokens: 0, trailing_tokens: 0, last_usage_index: None };
        assert_eq!(tokens_in_use(&none, &breakdown), (300, UsageSource::Estimate));
        let reported = ContextUsageEstimate { tokens: 900, usage_tokens: 900, trailing_tokens: 0, ..none };
        assert_eq!(tokens_in_use(&reported, &breakdown), (900, UsageSource::Response));
        let mixed = ContextUsageEstimate { tokens: 950, usage_tokens: 900, trailing_tokens: 50, ..none };
        assert_eq!(tokens_in_use(&mixed, &breakdown), (950, UsageSource::ResponseAndEstimate));
    }

    #[test]
    fn renders_a_table_with_a_dot_grid_and_free_space() {
        let breakdown = ContextBreakdown {
            system_prompt: 1_200,
            tool_definitions: 2_500,
            context_files: 400,
            skills: 0,
            user: 900,
            assistant: 3_000,
            tool_results: 12_000,
        };
        let report = ContextReport {
            model: Some("local/tiny".into()),
            window: 100_000,
            in_use: 20_000,
            source: UsageSource::Response,
            compact_at: Some(83_616),
            breakdown,
        };
        let text = report_text(&report, &plain);
        let used = vec!["●"; GRID_COLUMNS].join(" ");
        let free = vec!["○"; GRID_COLUMNS].join(" ");
        let expected = [
            "Context Usage",
            "",
            "Model:          local/tiny",
            "Window:         100k tokens",
            "In use:         20k tokens (20.0%), from the last response",
            "Auto-compact:   at 84k tokens (83.6%)",
            "",
            used.as_str(),
            free.as_str(),
            free.as_str(),
            free.as_str(),
            free.as_str(),
            "",
            "Estimated breakdown",
            "  ● System prompt          1.2k   1.2%",
            "  ● Tool definitions       2.5k   2.5%",
            "  ● Context files           400   0.4%",
            "  ● Skills                    0   0.0%",
            "  ● User messages           900   0.9%",
            "  ● Assistant messages     3.0k   3.0%",
            "  ● Tool results            12k  12.0%",
            "  ○ Free space              80k  80.0%",
        ];
        assert_eq!(text, expected.join("\n"));
    }

    #[test]
    fn renders_without_a_model_or_messages() {
        let breakdown = ContextBreakdown { system_prompt: 500, ..ContextBreakdown::default() };
        let report = ContextReport {
            model: None,
            window: 0,
            in_use: 500,
            source: UsageSource::Estimate,
            compact_at: None,
            breakdown,
        };
        let text = report_text(&report, &plain);
        assert!(text.contains("Model:          none selected\nWindow:         unknown\n"), "{text}");
        assert!(text.contains("In use:         500 tokens, estimated\n"), "{text}");
        assert!(!text.contains("Auto-compact") && !text.contains("Free space") && !text.contains('○'), "{text}");
        assert!(text.ends_with(&format!("  ● Tool results{}0", " ".repeat(14))), "{text}");
    }

    #[test]
    fn the_parts_share_the_used_dots_in_order() {
        let breakdown = ContextBreakdown {
            system_prompt: 1_200,
            tool_definitions: 2_500,
            context_files: 400,
            user: 900,
            assistant: 3_000,
            tool_results: 12_000,
            ..ContextBreakdown::default()
        };
        let dots = dot_parts(20_000, 100_000, &breakdown);
        let mut expected = vec![Some(0), Some(1), Some(1), Some(1), Some(4), Some(5), Some(5), Some(5)];
        expected.extend([Some(6); 12]);
        expected.extend([None; GRID_DOTS - 20]);
        assert_eq!(dots, expected);
        // A reported count larger than the estimate scales the parts up; a full window fills every dot.
        assert_eq!(dot_parts(40_000, 100_000, &breakdown).iter().flatten().count(), 40);
        assert!(dot_parts(250_000, 100_000, &breakdown).iter().all(Option::is_some));
        assert_eq!(dot_parts(1, 100_000, &breakdown)[..2], [Some(6), None]);
    }
}
