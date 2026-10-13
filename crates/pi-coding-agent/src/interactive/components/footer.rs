//! The footer: cwd, git branch, session name, token and cost totals, context use, and the model
//! with its thinking level (upstream `components/footer.ts`).

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use pi_ai::{Message, StopReason, ThinkingLevel};
use pi_tui::utils::truncate_to_width_with;
use pi_tui::{Component, visible_width};

use crate::agent_session::AgentSession;
use crate::compaction::ContextUsage;
use crate::config::home_dir;
use crate::interactive::footer_data::FooterData;
use crate::interactive::util::{format_tokens, resolve_lexically};
use crate::session::EntryData;
use crate::session::usage::UsageTotals;
use crate::theme::{ThemeColor, theme};

/// The `api` of virtual models, which route each request to a physical model.
pub const VIRTUAL_MODEL_API: &str = "pi-virtual";

/// Newlines, tabs, and repeated spaces collapse to single spaces.
fn sanitize_status_text(text: &str) -> String {
    text.split(['\r', '\n', '\t', ' ']).filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" ")
}

/// `cwd` with the home directory shown as `~`.
pub fn format_cwd_for_footer(cwd: &Path, home: Option<&Path>) -> String {
    let Some(home) = home else { return cwd.display().to_string() };
    let resolved_cwd = resolve_lexically(cwd, Path::new("/"));
    let resolved_home = resolve_lexically(home, Path::new("/"));
    match resolved_cwd.strip_prefix(&resolved_home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        Err(_) => cwd.display().to_string(),
    }
}

#[derive(Clone, PartialEq)]
struct StatsKey {
    session_id: String,
    leaf_id: Option<String>,
    entry_count: usize,
    limits_model: Option<(String, String)>,
}

struct SessionStats {
    key: StatsKey,
    session: usize,
    totals: UsageTotals,
    latest_cache_hit_rate: Option<f64>,
    context_usage: Option<ContextUsage>,
}

/// The physical model and level of the latest successful response under a virtual model.
fn routed_model(session: &AgentSession) -> Option<(String, Option<ThinkingLevel>)> {
    session.model().filter(|model| model.api == VIRTUAL_MODEL_API)?;
    let latest = session.with_state(|state| {
        state.messages.iter().rev().find_map(|message| match message {
            Message::Assistant(assistant)
                if assistant.stop_reason != StopReason::Error && assistant.stop_reason != StopReason::Aborted =>
            {
                Some((assistant.provider.clone(), assistant.model.clone(), assistant.thinking_level))
            }
            _ => None,
        })
    })?;
    let physical = session.model_runtime().get_model(&latest.0, &latest.1)?;
    Some((physical.id, latest.2))
}

pub struct FooterComponent {
    session: Arc<AgentSession>,
    data: Rc<RefCell<FooterData>>,
    auto_compact_enabled: bool,
    stats: Option<SessionStats>,
}

impl FooterComponent {
    pub fn new(session: Arc<AgentSession>, data: Rc<RefCell<FooterData>>) -> Self {
        Self { session, data, auto_compact_enabled: true, stats: None }
    }

    pub fn set_session(&mut self, session: Arc<AgentSession>) {
        self.session = session;
    }

    pub fn set_auto_compact_enabled(&mut self, enabled: bool) {
        self.auto_compact_enabled = enabled;
    }

    fn session_stats(&mut self) -> &SessionStats {
        let session = &self.session;
        let limits_model = routed_model(session)
            .and_then(|(id, _)| session.model().map(|model| (model.provider, id)))
            .or_else(|| session.model().map(|model| (model.provider, model.id)));
        let key = {
            let manager = session.session_manager();
            StatsKey {
                session_id: manager.id().to_string(),
                leaf_id: manager.leaf_id().map(String::from),
                entry_count: manager.entry_count(),
                limits_model,
            }
        };
        let session_ptr = Arc::as_ptr(session) as usize;
        if self.stats.as_ref().is_some_and(|stats| stats.key == key && stats.session == session_ptr) {
            return self.stats.as_ref().expect("checked above");
        }
        let mut totals = UsageTotals::default();
        let mut latest_cache_hit_rate = None;
        {
            let manager = session.session_manager();
            for entry in manager.entries() {
                match entry.data() {
                    EntryData::Usage(usage) => totals.add(&usage.usage),
                    EntryData::Message(Message::Assistant(assistant)) => {
                        let usage = &assistant.usage;
                        totals.add(usage);
                        let prompt = usage.input + usage.cache_read + usage.cache_write;
                        latest_cache_hit_rate = (prompt > 0).then(|| usage.cache_read as f64 / prompt as f64 * 100.0);
                    }
                    EntryData::Message(Message::ToolResult(result)) => {
                        if let Some(usage) = &result.usage {
                            totals.add(usage);
                        }
                    }
                    EntryData::Compaction(compaction) => {
                        if let Some(usage) = &compaction.usage {
                            totals.add(usage);
                        }
                    }
                    EntryData::BranchSummary(summary) => {
                        if let Some(usage) = &summary.usage {
                            totals.add(usage);
                        }
                    }
                    _ => {}
                }
            }
        }
        let context_usage = session.context_usage();
        self.stats = Some(SessionStats { key, session: session_ptr, totals, latest_cache_hit_rate, context_usage });
        self.stats.as_ref().expect("just set")
    }
}

impl Component for FooterComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme = theme();
        let (totals, cache_hit_rate, context_usage) = {
            let stats = self.session_stats();
            (stats.totals, stats.latest_cache_hit_rate, stats.context_usage)
        };
        let session = self.session.clone();
        let model = session.model();
        let context_window =
            context_usage.as_ref().map(|usage| usage.context_window).or(model.as_ref().map(|m| m.context_window));
        let percent = context_usage.as_ref().and_then(|usage| usage.percent);
        let percent_value = percent.unwrap_or(0.0);

        let mut pwd = format_cwd_for_footer(session.cwd(), Some(&home_dir()));
        let mut data = self.data.borrow_mut();
        if let Some(branch) = data.git_branch() {
            pwd = format!("{pwd} ({branch})");
        }
        if let Some(name) = session.session_name() {
            pwd = format!("{pwd} • {name}");
        }

        let mut parts = Vec::new();
        if totals.input > 0 {
            parts.push(format!("↑{}", format_tokens(totals.input)));
        }
        if totals.output > 0 {
            parts.push(format!("↓{}", format_tokens(totals.output)));
        }
        if totals.cache_read > 0 {
            parts.push(format!("R{}", format_tokens(totals.cache_read)));
        }
        if totals.cache_write > 0 {
            parts.push(format!("W{}", format_tokens(totals.cache_write)));
        }
        if (totals.cache_read > 0 || totals.cache_write > 0)
            && let Some(rate) = cache_hit_rate
        {
            parts.push(format!("CH{rate:.1}%"));
        }
        let subscription = model
            .as_ref()
            .is_some_and(|model| model.provider == "kimi-coding" || data.is_using_subscription(&model.provider));
        if totals.cost != 0.0 || subscription {
            parts.push(format!("${:.3}{}", totals.cost, if subscription { " (sub)" } else { "" }));
        }
        let auto = if self.auto_compact_enabled { " (auto)" } else { "" };
        let window = format_tokens(context_window.unwrap_or(0));
        let display = match percent {
            Some(percent) => format!("{percent:.1}%/{window}{auto}"),
            None => format!("?/{window}{auto}"),
        };
        parts.push(if percent_value > 90.0 {
            theme.fg(ThemeColor::Error, &display)
        } else if percent_value > 70.0 {
            theme.fg(ThemeColor::Warning, &display)
        } else {
            display
        });

        let mut stats_left = parts.join(" ");
        let mut stats_left_width = visible_width(&stats_left);
        if stats_left_width > width {
            stats_left = truncate_to_width_with(&stats_left, width, "...", false);
            stats_left_width = visible_width(&stats_left);
        }
        let model_name = model.as_ref().map_or("no-model".to_string(), |model| model.id.clone());
        let mut right = model_name.clone();
        if model.as_ref().is_some_and(|model| model.reasoning) {
            right = match session.thinking_level() {
                ThinkingLevel::Off => format!("{model_name} • thinking off"),
                level => format!("{model_name} • {}", level.as_str()),
            };
        }
        if let Some((routed, level)) = routed_model(&session) {
            let level = level.map(|level| format!(" • {}", level.as_str())).unwrap_or_default();
            right.push_str(&format!(" → {routed}{level}"));
        }
        let min_padding = 2;
        if data.available_provider_count() > 1
            && let Some(model) = &model
        {
            let with_provider = format!("({}) {right}", model.provider);
            if stats_left_width + min_padding + visible_width(&with_provider) <= width {
                right = with_provider;
            }
        }
        let right_width = visible_width(&right);
        let stats_line = if stats_left_width + min_padding + right_width <= width {
            format!("{stats_left}{}{right}", " ".repeat(width - stats_left_width - right_width))
        } else if width > stats_left_width + min_padding {
            let right = truncate_to_width_with(&right, width - stats_left_width - min_padding, "", false);
            let padding = width.saturating_sub(stats_left_width + visible_width(&right));
            format!("{stats_left}{}{right}", " ".repeat(padding))
        } else {
            stats_left.clone()
        };
        let remainder = &stats_line[stats_left.len()..];
        let pwd_line =
            truncate_to_width_with(&theme.fg(ThemeColor::Dim, &pwd), width, &theme.fg(ThemeColor::Dim, "..."), false);
        let mut lines = vec![
            pwd_line,
            format!("{}{}", theme.fg(ThemeColor::Dim, &stats_left), theme.fg(ThemeColor::Dim, remainder)),
        ];
        if !data.extension_statuses().is_empty() {
            let line =
                data.extension_statuses().values().map(|text| sanitize_status_text(text)).collect::<Vec<_>>().join(" ");
            lines.push(truncate_to_width_with(&line, width, &theme.fg(ThemeColor::Dim, "..."), false));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_cwd_relative_to_home() {
        let home = Path::new("/home/user");
        assert_eq!(format_cwd_for_footer(Path::new("/home/user"), Some(home)), "~");
        let inside = format_cwd_for_footer(Path::new("/home/user/project"), Some(home));
        assert_eq!(inside, format!("~{}project", std::path::MAIN_SEPARATOR));
        assert_eq!(format_cwd_for_footer(Path::new("/home/username"), Some(home)), "/home/username");
        assert_eq!(format_cwd_for_footer(Path::new("/srv"), None), "/srv");
    }

    #[test]
    fn sanitizes_status_text() {
        assert_eq!(sanitize_status_text(" a\n\tb  c\r"), "a b c");
    }
}
