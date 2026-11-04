//! The `/context` report (pi-rust extension): how much of the model's context window the
//! conversation uses, with an estimated breakdown of the next request by part.

use crate::agent_session::AgentSession;
use crate::agent_session::context_breakdown::ContextBreakdown;
use crate::compaction::{ContextUsageEstimate, estimate_projected_context_tokens};
use crate::interactive::util::format_tokens;

/// Each braille cell is empty, half (left column), or full, so the bar moves in half cells.
const BAR_CELLS: usize = 10;
const BAR_STEPS: usize = BAR_CELLS * 2;
const EMPTY_CELL: &str = "\u{2800}";
const HALF_CELL: &str = "⡇";
const FULL_CELL: &str = "⣿";
const LABEL_WIDTH: usize = 15;
const ROW_WIDTH: usize = 20;

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

pub struct ContextReport {
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
pub enum ContextStyle {
    Title,
    Label,
    /// A part of the breakdown, by its index in [`ContextBreakdown::parts`].
    Part(usize),
}

fn percent(tokens: u64, window: u64) -> String {
    format!("{:.1}%", tokens as f64 / window as f64 * 100.0)
}

/// The part that fills each half-cell step of the bar, or `None` for a free step. One step is
/// `1 / BAR_STEPS` of the window; the parts share the used steps in proportion to their size.
fn step_parts(in_use: u64, window: u64, breakdown: &ContextBreakdown) -> Vec<Option<usize>> {
    let steps = BAR_STEPS as u128;
    let used = ((in_use as u128 * steps + window as u128 / 2) / window as u128).min(steps) as usize;
    let total = breakdown.total().max(1) as u128;
    let mut ends = Vec::new();
    let mut sum = 0;
    for (_, _, tokens) in breakdown.parts() {
        sum += tokens as u128;
        ends.push(((sum * used as u128 + total / 2) / total) as usize);
    }
    let last = ends.len() - 1;
    (0..BAR_STEPS).map(|step| (step < used).then(|| ends.iter().position(|&end| end > step).unwrap_or(last))).collect()
}

/// A cell takes the color of the part that fills its left half.
fn usage_bar(
    in_use: u64,
    window: u64,
    breakdown: &ContextBreakdown,
    style: &dyn Fn(ContextStyle, &str) -> String,
) -> String {
    let steps = step_parts(in_use, window, breakdown);
    steps
        .chunks(2)
        .map(|cell| match (cell[0], cell[1]) {
            (Some(part), Some(_)) => style(ContextStyle::Part(part), FULL_CELL),
            (Some(part), None) => style(ContextStyle::Part(part), HALF_CELL),
            _ => EMPTY_CELL.to_string(),
        })
        .collect()
}

impl ContextReport {
    /// The report of `session`'s current context.
    pub fn of_session(session: &AgentSession) -> Self {
        let selected = session.model();
        let limits = session.limits_model();
        let breakdown = session.context_breakdown();
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
        Self { model, window, in_use, source, compact_at, breakdown }
    }

    pub fn text(&self, style: &dyn Fn(ContextStyle, &str) -> String) -> String {
        let window = self.window;
        let mut lines = vec![style(ContextStyle::Title, "Context Usage"), String::new()];
        let field = |label: &str, value: &str| {
            format!("{} {value}", style(ContextStyle::Label, &format!("{label:<LABEL_WIDTH$}")))
        };
        lines.push(field("Model:", self.model.as_deref().unwrap_or("none selected")));
        let shown_window = if window > 0 { format!("{} tokens", format_tokens(window)) } else { "unknown".into() };
        lines.push(field("Window:", &shown_window));
        let source = match self.source {
            UsageSource::Response => "from the last response",
            UsageSource::ResponseAndEstimate => "last response plus estimated new messages",
            UsageSource::Estimate => "estimated",
        };
        let share = if window > 0 { format!(" ({})", percent(self.in_use, window)) } else { String::new() };
        lines.push(field("In use:", &format!("{} tokens{share}, {source}", format_tokens(self.in_use))));
        if window > 0 {
            let compaction = match self.compact_at {
                Some(at) => format!("at {} tokens ({})", format_tokens(at), percent(at, window)),
                None => "off".into(),
            };
            lines.push(field("Auto-compact:", &compaction));
            lines.push(String::new());
            lines.push(usage_bar(self.in_use, window, &self.breakdown, style));
        }
        lines.push(String::new());
        lines.push(style(ContextStyle::Title, "Estimated breakdown"));
        let free = self.window.saturating_sub(self.in_use);
        let free_row = (window > 0).then_some(("Free space", free));
        for (index, (name, tokens)) in
            self.breakdown.parts().into_iter().map(|(_, name, tokens)| (name, tokens)).chain(free_row).enumerate()
        {
            let share = if window > 0 { format!(" {:>6}", percent(tokens, window)) } else { String::new() };
            let key =
                if name == "Free space" { EMPTY_CELL.to_string() } else { style(ContextStyle::Part(index), FULL_CELL) };
            lines.push(format!("  {key} {name:<ROW_WIDTH$}{:>7}{share}", format_tokens(tokens)));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(_: ContextStyle, text: &str) -> String {
        text.to_string()
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
    fn renders_a_table_with_a_braille_bar_and_free_space() {
        let breakdown = ContextBreakdown {
            system_prompt: 1_200,
            builtin_tools: 2_000,
            extension_tools: 300,
            mcp_tools: 200,
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
        let text = report.text(&plain);
        let bar = format!("⣿⣿{}", EMPTY_CELL.repeat(8));
        let expected = [
            "Context Usage",
            "",
            "Model:          local/tiny",
            "Window:         100k tokens",
            "In use:         20k tokens (20.0%), from the last response",
            "Auto-compact:   at 84k tokens (83.6%)",
            "",
            bar.as_str(),
            "",
            "Estimated breakdown",
            "  ⣿ System prompt          1.2k   1.2%",
            "  ⣿ Built-in tools         2.0k   2.0%",
            "  ⣿ Extension tools         300   0.3%",
            "  ⣿ MCP tools               200   0.2%",
            "  ⣿ Context files           400   0.4%",
            "  ⣿ Skills                    0   0.0%",
            "  ⣿ User messages           900   0.9%",
            "  ⣿ Assistant messages     3.0k   3.0%",
            "  ⣿ Tool results            12k  12.0%",
            "  \u{2800} Free space              80k  80.0%",
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
        let text = report.text(&plain);
        assert!(text.contains("Model:          none selected\nWindow:         unknown\n"), "{text}");
        assert!(text.contains("In use:         500 tokens, estimated\n"), "{text}");
        assert!(!text.contains("Auto-compact") && !text.contains("Free space") && !text.contains('\u{2800}'), "{text}");
        assert!(text.ends_with(&format!("  ⣿ Tool results{}0", " ".repeat(14))), "{text}");
    }

    #[test]
    fn the_bar_fills_in_half_cells_of_five_percent() {
        let breakdown = ContextBreakdown { tool_results: 1, ..ContextBreakdown::default() };
        let bar = |percent: u64| usage_bar(percent, 100, &breakdown, &plain).replace(EMPTY_CELL, "_");
        let expected = [
            (0, "__________"),
            (5, "⡇_________"),
            (10, "⣿_________"),
            (15, "⣿⡇________"),
            (25, "⣿⣿⡇_______"),
            (40, "⣿⣿⣿⣿______"),
            (50, "⣿⣿⣿⣿⣿_____"),
            (65, "⣿⣿⣿⣿⣿⣿⡇___"),
            (75, "⣿⣿⣿⣿⣿⣿⣿⡇__"),
            (90, "⣿⣿⣿⣿⣿⣿⣿⣿⣿_"),
            (95, "⣿⣿⣿⣿⣿⣿⣿⣿⣿⡇"),
            (100, "⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿"),
            (250, "⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿"),
        ];
        for (percent, shown) in expected {
            assert_eq!(bar(percent), shown, "{percent}%");
        }
    }

    #[test]
    fn the_parts_share_the_used_steps_in_order() {
        let breakdown = ContextBreakdown {
            system_prompt: 2_000,
            builtin_tools: 2_000,
            user: 1_000,
            assistant: 5_000,
            tool_results: 10_000,
            ..ContextBreakdown::default()
        };
        let steps = step_parts(20_000, 20_000, &breakdown);
        let mut expected = vec![Some(0), Some(0), Some(1), Some(1), Some(6)];
        expected.extend([Some(7); 5]);
        expected.extend([Some(8); 10]);
        assert_eq!(steps, expected);
        // A reported count larger than the estimate scales the parts up to the bar.
        assert_eq!(step_parts(10_000, 20_000, &breakdown).iter().flatten().count(), 10);
        assert_eq!(step_parts(10_000, 20_000, &breakdown)[..3], [Some(0), Some(1), Some(6)]);
    }
}
