use std::path::PathBuf;
use std::time::Duration;

use pi_ai::ThinkingLevel;
use pi_tui::visible_width;
use serde_json::json;

use super::command::{output_lines, run_command};
use super::external::{parse_codex_items, read_command};
use super::segments::{Item, ItemKind, segment_text};
use super::themes::{Palette, Role, ink_for};
use super::*;
use crate::interactive_components::test_support::plain;
use crate::theme::tests::global_state;

fn settings(value: serde_json::Value) -> StatusLineSettings {
    serde_json::from_value(value).expect("status line settings")
}

fn builtin(value: serde_json::Value) -> BuiltinStatusLine {
    match StatusLineConfig::from_settings(&settings(value), None) {
        Some(StatusLineConfig::Builtin(builtin)) => builtin,
        other => panic!("expected a builtin status line, got {other:?}"),
    }
}

fn first_line(value: serde_json::Value, width: usize) -> String {
    builtin(value).render(&snapshot(), width).into_iter().next().expect("a line")
}

fn plain_line(line: &str) -> String {
    plain(std::slice::from_ref(&line.to_string())).remove(0)
}

fn snapshot() -> StatusSnapshot {
    StatusSnapshot {
        cwd: PathBuf::from("/home/me/Workspace/node-cli"),
        home: Some(PathBuf::from("/home/me")),
        project: Some("node-cli".into()),
        branch: Some("format-double-quotes".into()),
        session_id: "0f3c9a7e-1111".into(),
        session_name: Some("quotes".into()),
        session_file: Some(PathBuf::from("/home/me/.pi/agent/sessions/s.jsonl")),
        totals: crate::session::usage::UsageTotals {
            input: 5100,
            output: 193,
            cache_read: 4500,
            cache_write: 0,
            cost: 0.013,
        },
        cache_hit_rate: Some(90.6),
        context: Some(crate::compaction::ContextUsage {
            tokens: Some(13_500),
            context_window: 500_000,
            percent: Some(2.7),
        }),
        context_window: Some(500_000),
        auto_compact: true,
        model: Some(StatusModel {
            provider: "openai-codex".into(),
            id: "gpt-6.1-sol".into(),
            name: "GPT 6.1 Sol".into(),
            reasoning: true,
        }),
        thinking: ThinkingLevel::High,
        routed: None,
        subscription: false,
        run_state: RunState::Working,
    }
}

#[test]
fn parses_command_and_builtin_settings() {
    let command = settings(json!({"type": "command", "command": "~/s.sh", "padding": 2, "refreshInterval": 0.5}));
    let Some(StatusLineConfig::Command(config)) = StatusLineConfig::from_settings(&command, None) else {
        panic!("expected a command status line");
    };
    assert_eq!((config.command.as_str(), config.padding, config.refresh), ("~/s.sh", 2, None));
    let refresh = settings(json!({"type": "command", "command": "s.sh", "refreshInterval": 5}));
    let Some(StatusLineConfig::Command(config)) = StatusLineConfig::from_settings(&refresh, None) else {
        panic!("expected a command status line");
    };
    assert_eq!(config.refresh, Some(Duration::from_secs(5)));
    for value in [json!({"type": "command"}), json!({"type": "rainbow"}), json!({"type": "off"})] {
        assert_eq!(StatusLineConfig::from_settings(&settings(value), None), None);
    }

    let minimal = builtin(json!({"type": "builtin"}));
    assert_eq!((minimal.chip, minimal.palette.clone(), minimal.separator.as_str()), (Chip::Plain, None, " • "));
    let custom = builtin(json!({"preset": "powerline", "chip": "rounded", "theme": "nord", "icons": "emoji",
        "segments": ["model", "nope", "git-branch", {"text": "λ", "color": "#ff0000"}, {"segment": "cost", "icon": "$"}]}));
    assert_eq!((custom.chip, custom.palette, custom.icons), (Chip::Rounded, Palette::named("nord"), Icons::Emoji));
    let kinds: Vec<ItemKind> = custom.lines[0].iter().map(|item| item.kind.clone()).collect();
    assert_eq!(
        kinds,
        [
            ItemKind::Segment(Segment::Model),
            ItemKind::Segment(Segment::Branch),
            ItemKind::Text("λ".into()),
            ItemKind::Segment(Segment::Cost)
        ]
    );
    assert_eq!(custom.lines[0][2].color.as_deref(), Some("#ff0000"));
    assert_eq!(custom.lines[0][3].icon.as_deref(), Some("$"));
    assert_eq!(builtin(json!({"style": "pills"})).chip, Chip::Block);
    let two_lines = builtin(json!({"segments": [["cwd", "branch"], ["tokens"], []]}));
    assert_eq!(two_lines.lines.len(), 2);
}

#[test]
fn every_preset_theme_and_chip_draws_a_line_that_fits() {
    let _theme = global_state();
    for preset in preset_names() {
        for width in [120, 30] {
            let lines = builtin(json!({"preset": preset})).render(&snapshot(), width);
            assert!(!lines.is_empty(), "{preset}");
            for line in &lines {
                assert!(visible_width(line) <= width, "{preset} at {width}: {line:?}");
            }
        }
    }
    for theme in theme_names() {
        assert!(Palette::named(theme).is_some(), "{theme}");
        let line = first_line(json!({"chip": "block", "theme": theme, "segments": ["dir", "model"]}), 60);
        assert!(line.contains("\x1b[48;"), "{theme} draws backgrounds: {line:?}");
    }
    for chip in ["plain", "block", "rounded", "powerline", "slanted", "brackets", "dot"] {
        let line = first_line(json!({"chip": chip, "theme": "dracula", "segments": ["dir", "fill", "model"]}), 60);
        assert_eq!(visible_width(&line), 60, "{chip}: {line:?}");
        assert!(plain_line(&line).contains("node-cli"), "{chip}");
    }
}

#[test]
fn chips_have_their_shapes() {
    let _theme = global_state();
    let segments = json!(["dir", "fill", "model"]);
    let shape = |chip: &str| plain_line(&first_line(json!({"chip": chip, "icons": "none", "segments": segments}), 40));
    assert!(shape("brackets").starts_with("[node-cli]") && shape("brackets").ends_with("[gpt-6.1-sol]"));
    assert!(shape("dot").starts_with("● node-cli") && shape("dot").ends_with("● gpt-6.1-sol"));
    assert!(shape("block").starts_with(" node-cli ") && shape("block").ends_with(" gpt-6.1-sol "));
    assert!(shape("rounded").starts_with("\u{e0b6} node-cli \u{e0b4}"));
    assert!(shape("powerline").starts_with(" node-cli \u{e0b0}") && shape("powerline").contains("\u{e0b2} gpt"));
    assert!(shape("slanted").starts_with(" node-cli \u{e0bc}") && shape("slanted").contains("\u{e0ba} gpt"));
}

#[test]
fn a_long_right_side_is_cut_and_the_chip_picks_the_separator() {
    let _theme = global_state();
    let line = plain_line(&first_line(json!({"preset": "cursor"}), 40));
    assert!(line.starts_with("gpt-6.1-sol · 3% used ") && line.ends_with('…'), "{line:?}");
    assert_eq!(visible_width(&line), 40);
    let block = plain_line(&first_line(json!({"chip": "block", "icons": "none", "segments": ["dir", "model"]}), 60));
    assert!(block.starts_with(" node-cli   gpt-6.1-sol "), "{block:?}");
}

#[test]
fn minimal_preset_puts_the_model_left_and_the_directory_right() {
    let _theme = global_state();
    let line = plain_line(&first_line(json!({"type": "builtin"}), 80));
    assert!(line.starts_with("gpt-6.1-sol • high • 2.7%/500k (auto) • $0.013"), "{line:?}");
    assert!(line.ends_with("node-cli • ⎇ format-double-quotes"), "{line:?}");
    assert_eq!(visible_width(&line), 80);
}

#[test]
fn segments_show_what_the_footer_shows() {
    let _theme = global_state();
    let text = |segment: Segment| segment_text(segment, &snapshot());
    let cwd = format!("~{}Workspace{}node-cli", std::path::MAIN_SEPARATOR, std::path::MAIN_SEPARATOR);
    assert_eq!(text(Segment::Cwd), Some(cwd));
    assert_eq!(text(Segment::Project).as_deref(), Some("node-cli"));
    assert_eq!(text(Segment::SessionId).as_deref(), Some("0f3c9a7e"));
    assert_eq!(text(Segment::ModelWithReasoning).as_deref(), Some("gpt-6.1-sol high"));
    assert_eq!(text(Segment::Tokens).as_deref(), Some("↑5.1k ↓193"));
    assert_eq!(text(Segment::TokensUsed).as_deref(), Some("9.8k used"));
    assert_eq!(text(Segment::Cache).as_deref(), Some("R4.5k CH90.6%"));
    assert_eq!(text(Segment::ContextUsed).as_deref(), Some("3% used"));
    assert_eq!(text(Segment::ContextRemaining).as_deref(), Some("97% left"));
    assert_eq!(text(Segment::ContextWindow).as_deref(), Some("500k window"));
    assert_eq!(text(Segment::ContextBar).as_deref(), Some("▱▱▱▱▱▱▱▱ 3%"));
    assert_eq!(text(Segment::RunState).as_deref(), Some("● working"));
    assert_eq!(text(Segment::Logo).as_deref(), Some("π"));
    assert_eq!(text(Segment::Version), Some(format!("v{}", env!("CARGO_PKG_VERSION"))));
    assert_eq!(text(Segment::Time).map(|time| time.len()), Some(5));
    assert_eq!(text(Segment::Fill), None);
    let mut full = snapshot();
    full.context = Some(crate::compaction::ContextUsage { tokens: None, context_window: 500_000, percent: Some(95.0) });
    full.model.as_mut().expect("model").reasoning = false;
    assert_eq!(segment_text(Segment::Thinking, &full), None);
    assert_eq!(segment_text(Segment::ContextBar, &full).as_deref(), Some("▰▰▰▰▰▰▰▰ 95%"));
    assert_eq!(Segment::ContextBar.role(&full), Role::ContextHigh);
    for segment in Segment::ALL {
        assert_eq!(Segment::from_name(segment.name()), Some(*segment));
    }
}

#[test]
fn codex_items_map_to_segments() {
    let toml = "model = \"gpt\" # main\n[tui]\nnotifications = true\nstatus_line = [\n  \"model-with-reasoning\", # model\n  'context-remaining', \"current-dir\",\n  \"git-branch\", \"weekly-limit\"\n]\n[mcp]\nstatus_line = [\"nope\"]\n";
    let items = parse_codex_items(toml).expect("items");
    assert_eq!(items, ["model-with-reasoning", "context-remaining", "current-dir", "git-branch", "weekly-limit"]);
    assert_eq!(parse_codex_items("tui.status_line = [\"model\"]").expect("dotted"), ["model"]);
    assert_eq!(parse_codex_items("[tui]\nanimations = false\n"), None);
    let _theme = global_state();
    let codex = BuiltinStatusLine::from_codex(&items, 0);
    let line = plain_line(&codex.render(&snapshot(), 120)[0]);
    let cwd = format!("~{}Workspace{}node-cli", std::path::MAIN_SEPARATOR, std::path::MAIN_SEPARATOR);
    assert_eq!(line, format!("gpt-6.1-sol high · 97% left · {cwd} · format-double-quotes"));
}

#[test]
fn reads_claude_and_cursor_commands() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("settings.json");
    std::fs::write(&path, r#"{"statusLine": {"type": "command", "command": " ~/.claude/s.sh ", "padding": 1}}"#)
        .unwrap();
    let found = read_command(&path).expect("command");
    assert_eq!((found.command.as_str(), found.padding, found.refresh_interval), ("~/.claude/s.sh", Some(1.0), None));
    std::fs::write(&path, r#"{"statusLine": {"type": "static", "command": "x"}}"#).unwrap();
    assert_eq!(read_command(&path), None);
    assert_eq!(read_command(&dir.path().join("missing.json")), None);
}

#[test]
fn colors_accept_tokens_and_hex_and_ink_follows_contrast() {
    let _theme = global_state();
    let line = first_line(json!({"segments": ["dir", "model"], "colors": {"dir": "#ff8800", "model": "accent"}}), 40);
    assert!(line.contains("\x1b[38;2;255;136;0m") || line.contains("\x1b[38;5;"), "{line:?}");
    assert!(plain_line(&line).starts_with("node-cli • gpt-6.1-sol"));
    let padded = builtin(json!({"segments": ["dir"], "padding": 3})).render(&snapshot(), 40);
    assert_eq!(plain(&padded), ["   node-cli"]);
    assert_eq!(ink_for([245, 245, 245]), [27, 27, 27]);
    assert_eq!(ink_for([20, 20, 40]), [245, 245, 245]);
    assert!(Item::parse(&json!({"color": "#fff"})).is_none());
}

#[test]
fn payload_uses_claude_code_field_names() {
    let payload = status_payload(&snapshot());
    assert_eq!(payload["hook_event_name"], "Status");
    assert_eq!(payload["cwd"], "/home/me/Workspace/node-cli");
    assert_eq!(payload["workspace"]["current_dir"], payload["cwd"]);
    assert_eq!(payload["model"]["id"], "gpt-6.1-sol");
    assert_eq!(payload["model"]["display_name"], "GPT 6.1 Sol");
    assert_eq!(payload["model"]["provider"], "openai-codex");
    assert_eq!(payload["cost"]["total_cost_usd"], 0.013);
    assert_eq!(payload["context_window"]["context_window_size"], 500_000);
    assert_eq!(payload["context_window"]["used_percentage"], 2.7);
    assert_eq!(payload["context_window"]["remaining_percentage"], 97.3);
    assert_eq!(payload["thinking"], json!({"enabled": true, "level": "high"}));
    assert_eq!(payload["git"], json!({"branch": "format-double-quotes", "project": "node-cli"}));
    assert_eq!(payload["run_state"], "working");
    assert_eq!(payload["tokens"]["cache_read"], 4500);
    assert_eq!(payload["transcript_path"], "/home/me/.pi/agent/sessions/s.jsonl");
}

#[test]
fn output_lines_drop_trailing_blank_lines() {
    assert_eq!(output_lines("a\tb\r\n\nc\n\n  \n"), ["a    b", "", "c"]);
    assert_eq!(output_lines(&"x\n".repeat(20)).len(), 8);
    assert!(output_lines("").is_empty());
}

#[tokio::test]
async fn command_reads_the_payload_and_sees_the_width() {
    let config = CommandConfig {
        command: r#"read -r json; printf '%s|%s\n' "$COLUMNS" "${json:0:2}""#.into(),
        padding: 0,
        refresh: None,
        shell_path: None,
    };
    let payload = status_payload(&snapshot()).to_string();
    let lines = run_command(config, payload, 77, std::env::temp_dir()).await.expect("command ran");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("77|{\""), "{lines:?}");
}

#[test]
fn layout_modes_remove_gaps_and_keep_narrow_lines_bounded() {
    let _theme = global_state();
    let config = json!({"chip": "plain", "icons": "none", "separator": " | ",
        "segments": ["dir", "fill", "model"]});
    let mut compact = config.clone();
    compact["layout"] = json!("compact");
    assert_eq!(plain_line(&first_line(compact.clone(), 120)), "node-cli | gpt-6.1-sol");
    let mut stacked = config;
    stacked["layout"] = json!("stacked");
    assert_eq!(plain(&builtin(stacked.clone()).render(&snapshot(), 120)), ["node-cli", "gpt-6.1-sol"]);
    for config in [compact, stacked] {
        for width in 0..40 {
            assert!(builtin(config.clone()).render(&snapshot(), width).iter().all(|line| visible_width(line) <= width));
        }
    }
    let mut no_branch = snapshot();
    no_branch.branch = None;
    let line = builtin(json!({"segments": ["branch", "fill", {"text": ""}, "model"], "icons": "none"}));
    assert_eq!(plain(&line.render(&no_branch, 120)), ["gpt-6.1-sol"]);
    assert!(builtin(json!({"segments": []})).render(&snapshot(), 120).is_empty());
    assert!(builtin(json!({"padding": 3})).render(&snapshot(), 2).is_empty());
}

#[test]
fn context_headroom_and_unknown_usage_are_not_fabricated() {
    let mut data = snapshot();
    assert_eq!(segment_text(Segment::ContextTokens, &data).as_deref(), Some("14k context"));
    assert_eq!(segment_text(Segment::ContextHeadroom, &data).as_deref(), Some("487k tokens left"));
    assert_eq!(status_payload(&data)["context_window"]["remaining_tokens"], 486_500);
    data.context.as_mut().unwrap().tokens = Some(600_000);
    assert_eq!(data.context_headroom(), Some(0));
    data.context = None;
    assert_eq!(segment_text(Segment::ContextBar, &data).as_deref(), Some("context ?"));
    assert_eq!(segment_text(Segment::ContextRemaining, &data), None);
    assert_eq!(segment_text(Segment::ContextHeadroom, &data), None);
    assert_eq!(segment_text(Segment::ContextTokens, &data), None);
    assert!(status_payload(&data)["context_window"]["remaining_tokens"].is_null());
}

#[test]
fn statusline_command_switches_presets_and_keeps_the_command() {
    let current = json!({"type": "command", "command": "s.sh", "chip": "dot", "theme": "nord", "segments": ["dir"], "colors": {"dir": "#fff"}});
    let pills = status_line_choice(Some(current), "pills").expect("pills");
    assert_eq!(pills, json!({"type": "builtin", "preset": "pills", "command": "s.sh", "colors": {"dir": "#fff"}}));
    assert_eq!(status_line_choice(Some(pills.clone()), "command").expect("command")["type"], "command");
    assert_eq!(status_line_choice(Some(pills.clone()), "from-claude").expect("claude")["type"], "claude");
    assert_eq!(status_line_choice(Some(pills.clone()), "claude").expect("claude look")["preset"], "claude");
    assert_eq!(status_line_choice(Some(pills), "off").expect("off")["type"], "off");
    assert!(status_line_choice(None, "command").is_err());
    assert!(status_line_choice(None, "rainbow").unwrap_err().contains("retro"));
    assert_eq!(status_line_choice(None, "from-codex").expect("codex"), json!({"type": "codex"}));
    assert_eq!(status_line_choice(None, "rounded").expect("rounded"), json!({"type": "builtin", "preset": "rounded"}));

    assert_eq!(status_line_name(None), "default footer");
    assert_eq!(status_line_name(Some(&settings(json!({"type": "builtin"})))), "minimal");
    assert_eq!(status_line_name(Some(&settings(json!({"preset": "powerline"})))), "powerline");
    assert_eq!(status_line_name(Some(&settings(json!({"type": "cursor"})))), "from-cursor");
    assert_eq!(status_line_name(Some(&settings(json!({"type": "off"})))), "off");
}
