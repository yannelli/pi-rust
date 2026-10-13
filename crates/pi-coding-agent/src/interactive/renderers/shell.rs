//! Shell tool renderers, shared by bash (`$`) and powershell (`PS>`) (upstream `renderers/bash.ts`).

use std::time::{Duration, Instant};

use pi_tui::ui_loop::{self, TimerId};
use pi_tui::{ComponentRef, Container, Spacer, component};
use serde_json::Value;

use super::{ToolRenderContext, ToolRenderer, ToolResultView, get_text_output, invalid_arg_text, str_arg, text};
use crate::interactive::components::visual_truncate::{Keep, VisualLinePreview};
use crate::interactive_components::key_hint;
use crate::theme::{ThemeColor, theme};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, format_size};

const BASH_PREVIEW_LINES: usize = 5;

/// `1.5s`, `2m 5s`, or `1h 2m 5s`.
pub fn format_duration(ms: u64) -> String {
    let seconds = ms as f64 / 1000.0;
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    let total = seconds.floor() as u64;
    let (minutes, remainder) = (total / 60, total % 60);
    if minutes < 60 {
        return format!("{minutes}m {remainder}s");
    }
    format!("{}h {}m {remainder}s", minutes / 60, minutes % 60)
}

#[derive(Default)]
struct ShellState {
    started_at: Option<Instant>,
    ended_at: Option<Instant>,
    interval: Option<TimerId>,
}

impl Drop for ShellState {
    fn drop(&mut self) {
        if let Some(interval) = self.interval.take() {
            ui_loop::clear_timer(interval);
        }
    }
}

pub struct ShellRenderer {
    pub prompt: &'static str,
}

fn format_shell_call(args: &Value, prompt: &str) -> String {
    let theme = theme();
    let timeout = args.get("timeout").and_then(Value::as_f64).filter(|timeout| *timeout != 0.0);
    let timeout_suffix = timeout
        .map(|timeout| {
            theme.fg(ThemeColor::Muted, &format!(" (timeout {})", crate::interactive::util::js_number(timeout)))
        })
        .unwrap_or_default();
    let command = match str_arg(args, "command") {
        None => invalid_arg_text(),
        Some(command) if command.is_empty() => theme.fg(ThemeColor::ToolOutput, "..."),
        Some(command) => command,
    };
    theme.fg(ThemeColor::ToolTitle, &theme.bold(&format!("{prompt} {command}"))) + &timeout_suffix
}

fn u64_detail(value: Option<&Value>, key: &str) -> Option<u64> {
    value?.get(key)?.as_u64()
}

impl ToolRenderer for ShellRenderer {
    fn render_call(&self, context: &mut ToolRenderContext<'_>) -> ComponentRef {
        let state = context.state.get_or_default::<ShellState>();
        if context.execution_started && state.started_at.is_none() {
            state.started_at = Some(Instant::now());
            state.ended_at = None;
        }
        text(&format_shell_call(context.args, self.prompt))
    }

    fn render_result(&self, result: &ToolResultView, context: &mut ToolRenderContext<'_>) -> Option<ComponentRef> {
        let invalidate = context.invalidate.clone();
        let (is_partial, is_error) = (context.is_partial, context.is_error);
        let state = context.state.get_or_default::<ShellState>();
        if state.started_at.is_some() && is_partial && state.interval.is_none() {
            state.interval = Some(ui_loop::set_interval(Duration::from_secs(1), move || invalidate()));
        }
        if !is_partial || is_error {
            state.ended_at.get_or_insert_with(Instant::now);
            if let Some(interval) = state.interval.take() {
                ui_loop::clear_timer(interval);
            }
        }
        let (started_at, ended_at) = (state.started_at, state.ended_at);
        let theme = theme();
        let mut container = Container::new();
        let mut output = get_text_output(result, context.show_images).trim().to_string();
        let truncation = result.detail("truncation");
        let truncated = truncation.and_then(|t| t.get("truncated")).and_then(Value::as_bool).unwrap_or(false);
        let full_output_path = result.detail("fullOutputPath").and_then(Value::as_str);
        if !is_partial
            && truncated
            && let Some(path) = full_output_path
            && output.ends_with(']')
            && let Some(start) = output.rfind("\n\n[")
            && output[start..].contains(path)
        {
            output = output[..start].trim_end().to_string();
        }
        if !output.is_empty() {
            let styled =
                output.split('\n').map(|line| theme.fg(ThemeColor::ToolOutput, line)).collect::<Vec<_>>().join("\n");
            if context.expanded {
                container.add_child(text(&format!("\n{styled}")));
            } else {
                container.add_child(component(Spacer::new(1)));
                let preview = VisualLinePreview::new(styled, BASH_PREVIEW_LINES, Keep::End, |hidden| {
                    let theme = crate::theme::theme();
                    format!(
                        "{} {}{}",
                        theme.fg(ThemeColor::Muted, &format!("... ({hidden} earlier lines,")),
                        key_hint("app.tools.expand", "to expand"),
                        theme.fg(ThemeColor::Muted, ")")
                    )
                });
                container.add_child(component(preview));
            }
        }
        if truncated || full_output_path.is_some() {
            let mut warnings = Vec::new();
            if let Some(path) = full_output_path {
                warnings.push(format!("Full output: {path}"));
            }
            if truncated {
                let output_lines = u64_detail(truncation, "outputLines").unwrap_or(0);
                if truncation.and_then(|t| t.get("truncatedBy")).and_then(Value::as_str) == Some("lines") {
                    let total = u64_detail(truncation, "totalLines").unwrap_or(0);
                    warnings.push(format!("Truncated: showing {output_lines} of {total} lines"));
                } else {
                    let max = u64_detail(truncation, "maxBytes").map_or(DEFAULT_MAX_BYTES, |bytes| bytes as usize);
                    warnings.push(format!("Truncated: {output_lines} lines shown ({} limit)", format_size(max)));
                }
            }
            container.add_child(text(&format!(
                "\n{}",
                theme.fg(ThemeColor::Warning, &format!("[{}]", warnings.join(". ")))
            )));
        }
        if let (false, Some(duration)) = (is_partial, context.duration_ms) {
            container.add_child(text(&format!(
                "\n{}",
                theme.fg(ThemeColor::Muted, &format!("Took {}", format_duration(duration)))
            )));
        } else if let Some(started) = started_at {
            let label = if is_partial { "Elapsed" } else { "Took" };
            let elapsed = ended_at.unwrap_or_else(Instant::now).duration_since(started).as_millis() as u64;
            container.add_child(text(&format!(
                "\n{}",
                theme.fg(ThemeColor::Muted, &format!("{label} {}", format_duration(elapsed)))
            )));
        }
        Some(component(container))
    }
}

#[cfg(test)]
mod tests {
    use super::format_duration;

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(1500), "1.5s");
        assert_eq!(format_duration(125_000), "2m 5s");
        assert_eq!(format_duration(3_725_000), "1h 2m 5s");
    }
}
