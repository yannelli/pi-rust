//! `/session`, `/debug`, and `/share` (upstream `handleSessionCommand`, `handleDebugCommand`, and
//! `session-share.ts`).

use std::cell::RefCell;
use std::rc::Rc;

use super::components::bordered_loader::BorderedLoader;
use super::components::summaries::format_thousands;
use super::mode::InteractiveMode;
use super::util::format_tokens;
use crate::cache_stats::compute_cache_waste;
use crate::cache_warmer::format_cache_warming_status;
use crate::session::usage::usage_cost_breakdown;
use crate::settings::schema::CacheWarmingMode;
use crate::theme::{ThemeColor, theme};

impl InteractiveMode {
    pub(super) fn handle_session_command(&self) {
        let session = self.session();
        let stats = session.session_stats().stats;
        let name = session.session_name();
        let (breakdown, waste) = {
            let manager = session.session_manager();
            let runtime = session.model_runtime().clone();
            (usage_cost_breakdown(manager.entries()), compute_cache_waste(manager.entries(), runtime.as_ref()))
        };
        let warming = match session.settings().cache_warming_mode() {
            CacheWarmingMode::Off => "off",
            CacheWarmingMode::Streaming => "streaming",
            CacheWarmingMode::Idle => "idle",
        };
        let warming_status = session.cache_warming_status();
        let warming_text = format_cache_warming_status(&warming_status, pi_ai::now_ms());
        let economics = warming_status
            .decision
            .as_ref()
            .filter(|decision| decision.economics_available)
            .map(|decision| (decision.miss_cost, decision.warm_cost));
        let selected = session.model().map(|model| format!("{}/{}", model.provider, model.id)).unwrap_or_default();
        let render = move || {
            let t = theme();
            let dim = |text: &str| t.fg(ThemeColor::Dim, text);
            let mut info = format!("{}\n\n", t.bold("Session Info"));
            if let Some(name) = &name {
                info.push_str(&format!("{} {name}\n", dim("Name:")));
            }
            let file = stats.session_file.as_ref().map(|file| file.display().to_string());
            info.push_str(&format!("{} {}\n", dim("File:"), file.as_deref().unwrap_or("In-memory")));
            info.push_str(&format!("{} {}\n\n", dim("ID:"), stats.session_id));
            info.push_str(&format!("{}\n", t.bold("Messages")));
            info.push_str(&format!("{} {}\n", dim("Total:"), stats.total_messages));
            info.push_str(&format!("{} {}\n", dim("User:"), stats.user_messages));
            info.push_str(&format!("{} {}\n", dim("Assistant:"), stats.assistant_messages));
            let tools = format!("{} calls, {} results", stats.tool_calls, stats.tool_results);
            info.push_str(&format!("{} {tools}\n\n", dim("Tools:")));
            info.push_str(&format!("{}\n", t.bold("Tokens")));
            let tokens = &stats.tokens;
            let prompt = tokens.input + tokens.cache_read + tokens.cache_write;
            info.push_str(&format!("{} {}\n", dim("Input:"), format_thousands(prompt)));
            if prompt > 0 && (tokens.cache_read > 0 || tokens.cache_write > 0) {
                let rate = dim(&format!("({:.1}%)", tokens.cache_read as f64 / prompt as f64 * 100.0));
                info.push_str(&format!("  {} {} {rate}\n", dim("Cached:"), format_thousands(tokens.cache_read)));
                let written = if tokens.cache_write > 0 {
                    format!(" {}", dim(&format!("({} written to cache)", format_thousands(tokens.cache_write))))
                } else {
                    String::new()
                };
                let uncached = format_thousands(tokens.input + tokens.cache_write);
                info.push_str(&format!("  {} {uncached}{written}\n", dim("Uncached:")));
            }
            info.push_str(&format!("{} {}\n", dim("Output:"), format_thousands(tokens.output)));
            info.push_str(&format!("{} {}\n", dim("Total:"), format_thousands(tokens.tokens())));
            info.push_str(&format!("\n{}\n", t.bold("Cache Warming")));
            info.push_str(&format!("{} {warming}\n", dim("Mode:")));
            info.push_str(&format!("{} {warming_text}\n", dim("Status:")));
            if let Some((miss, warm)) = economics {
                info.push_str(&format!("{} ${miss:.3}\n", dim("Cache miss penalty:")));
                info.push_str(&format!("{} ${warm:.3}\n", dim("Refresh cost:")));
            }
            if stats.cost > 0.0 || waste.missed_tokens > 0 {
                info.push_str(&format!("\n{}\n", t.bold("Cost")));
                info.push_str(&format!("{} ${:.3}", dim("Total:"), stats.cost));
                if breakdown.len() > 1 || breakdown.first().is_some_and(|entry| entry.key != selected) {
                    for entry in &breakdown {
                        let tokens = dim(&format!("({} tokens)", format_tokens(entry.tokens)));
                        let key = dim(&format!("{}:", entry.key));
                        info.push_str(&format!("\n  {key} ${:.3} {tokens}", entry.cost));
                    }
                }
                if waste.missed_tokens > 0 {
                    let misses = match waste.miss_count {
                        1 => "1 miss".to_string(),
                        count => format!("{count} misses"),
                    };
                    let detail = format!("{} tokens, {misses}", format_thousands(waste.missed_tokens));
                    let label = dim("Cache Re-billed:");
                    if waste.missed_cost >= 0.0001 {
                        let detail = dim(&format!("({detail})"));
                        info.push_str(&format!("\n{label} ${:.3} {detail}", waste.missed_cost));
                    } else {
                        info.push_str(&format!("\n{label} {detail}"));
                    }
                }
            }
            info
        };
        self.add_spacer();
        self.add_text(render, 1, 0);
        self.request_render();
    }

    /// `/debug` and its key: write the rendered lines and the messages to the debug log.
    pub(super) fn handle_debug_command(&self) {
        let tui = self.tui();
        let (width, height) = {
            let terminal = self.terminal.borrow();
            (terminal.columns(), terminal.rows())
        };
        let lines = tui.render(width);
        let path = self.agent_dir.join(format!("{}-debug.log", crate::resources::brand::APP_NAME));
        let mut data = vec![
            format!("Debug output at {}", jiff::Timestamp::now()),
            format!("Terminal: {width}x{height}"),
            format!("Total lines: {}", lines.len()),
            String::new(),
            "=== All rendered lines with visible widths ===".to_string(),
        ];
        for (index, line) in lines.iter().enumerate() {
            let escaped = serde_json::Value::from(line.as_str());
            data.push(format!("[{index}] (w={}) {escaped}", pi_tui::visible_width(line)));
        }
        data.push(String::new());
        data.push("=== Agent messages (JSONL) ===".to_string());
        for message in self.session().messages() {
            data.push(serde_json::to_string(&message).unwrap_or_default());
        }
        data.push(String::new());
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(error) = std::fs::write(&path, data.join("\n")) {
            self.show_error(&format!("Failed to write debug log: {error}"));
            return;
        }
        let shown = path.display().to_string();
        self.add_spacer();
        let written = move || {
            let t = theme();
            format!("{}\n{}", t.fg(ThemeColor::Accent, "✓ Debug log written"), t.fg(ThemeColor::Muted, &shown))
        };
        self.add_text(written, 1, 1);
        self.request_render();
    }

    /// `/share`: a secret GitHub gist of the HTML export, through the `gh` command.
    pub(super) fn handle_share_command(&self) {
        match std::process::Command::new("gh").args(["auth", "status"]).output() {
            Err(_) => {
                return self.show_error("GitHub CLI (gh) is not installed. Install it from https://cli.github.com/");
            }
            Ok(output) if !output.status.success() => {
                return self.show_error("GitHub CLI is not logged in. Run 'gh auth login' first.");
            }
            Ok(_) => {}
        }
        let directory = std::env::temp_dir().join(format!("pi-share-{}", crate::session::ids::uuid_v4()));
        let html = directory.join("session.html");
        if let Err(error) = std::fs::create_dir_all(&directory)
            .map_err(anyhow::Error::from)
            .and_then(|()| self.session().export_to_html(Some(&html.to_string_lossy())))
        {
            let _ = std::fs::remove_dir_all(&directory);
            return self.show_error(&format!("Failed to export session: {error}"));
        }
        let loader = Rc::new(RefCell::new(BorderedLoader::new(&self.tui(), "Creating gist...", true)));
        let cancel = loader.borrow().signal();
        let weak = self.weak.clone();
        loader.borrow_mut().set_on_abort(Some(Box::new(move || {
            if let Some(mode) = weak.upgrade() {
                mode.selector.restore_editor();
                mode.show_status("Share cancelled");
            }
        })));
        self.show_in_editor_slot(loader.clone(), loader.clone());
        self.spawn(move |mode| async move {
            let mut command = tokio::process::Command::new("gh");
            command.args(["gist", "create", "--public=false"]).arg(&html).kill_on_drop(true);
            let output = tokio::select! {
                output = command.output() => output,
                _ = cancel.cancelled() => Err(std::io::Error::other("cancelled")),
            };
            let _ = std::fs::remove_dir_all(&directory);
            if cancel.is_cancelled() {
                return;
            }
            loader.borrow_mut().dispose();
            mode.selector.restore_editor();
            let output = match output {
                Ok(output) => output,
                Err(error) => return mode.show_error(&format!("Failed to create gist: {error}")),
            };
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let reason = if stderr.is_empty() { "Unknown error".to_string() } else { stderr };
                return mode.show_error(&format!("Failed to create gist: {reason}"));
            }
            let gist_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let Some(gist_id) = gist_url.rsplit('/').next().filter(|id| !id.is_empty()) else {
                return mode.show_error("Failed to parse gist ID from gh output");
            };
            let preview = crate::export::share::share_viewer_url(gist_id);
            let link = pi_tui::terminal_image::hyperlink;
            let preview_link = link(&preview, &preview);
            mode.show_status(&format!("Share URL: {preview_link}\nGist: {}", link(&gist_url, &gist_url)));
        });
    }
}
