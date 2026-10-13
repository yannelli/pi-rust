//! Renderer output, ported from upstream `tool-execution-component.test.ts` and
//! `tool-renderer-examples.test.ts`.

use std::path::Path;
use std::rc::Rc;

use pi_ai::ContentBlock;
use serde_json::{Value, json};

use super::*;

fn plain(lines: &[String]) -> Vec<String> {
    lines.iter().map(|line| pi_tui::strip_terminal_sequences(line).trim_end().to_string()).collect()
}

struct Call<'a> {
    renderer: Rc<dyn ToolRenderer>,
    args: Value,
    cwd: &'a Path,
    state: RendererState,
    expanded: bool,
}

impl<'a> Call<'a> {
    fn new(tool: &str, args: Value, cwd: &'a Path) -> Self {
        Self::with(builtin_renderer(tool).unwrap(), args, cwd)
    }

    fn with(renderer: Rc<dyn ToolRenderer>, args: Value, cwd: &'a Path) -> Self {
        Self { renderer, args, cwd, state: RendererState::default(), expanded: false }
    }

    fn context(&mut self, partial: bool, is_error: bool) -> ToolRenderContext<'_> {
        ToolRenderContext {
            args: &self.args,
            tool_call_id: "call-1",
            invalidate: Rc::new(|| {}),
            state: &mut self.state,
            cwd: self.cwd,
            execution_started: true,
            args_complete: true,
            is_partial: partial,
            expanded: self.expanded,
            show_images: true,
            is_error,
            duration_ms: Some(1500),
            output_pad: 1,
        }
    }

    fn header(&mut self) -> Vec<String> {
        let renderer = self.renderer.clone();
        let component = renderer.render_call(&mut self.context(false, false));
        let lines = component.borrow_mut().render(80);
        plain(&lines)
    }

    fn result(&mut self, result: &ToolResultView) -> Vec<String> {
        let renderer = self.renderer.clone();
        let component = renderer.render_result(result, &mut self.context(false, result.is_error)).unwrap();
        let lines = component.borrow_mut().render(80);
        plain(&lines)
    }
}

fn output(text: &str) -> ToolResultView {
    ToolResultView { content: vec![ContentBlock::text(text)], ..ToolResultView::default() }
}

#[test]
fn shell_headers_show_prompt_command_and_timeout() {
    let cwd = Path::new("/tmp");
    assert_eq!(Call::new("bash", json!({"command": "ls -la", "timeout": 30}), cwd).header(), ["$ ls -la (timeout 30)"]);
    assert_eq!(Call::new("powershell", json!({"command": "dir"}), cwd).header(), ["PS> dir"]);
    assert_eq!(Call::new("bash", json!({"command": 5}), cwd).header(), ["$ [invalid arg]"]);
    assert_eq!(Call::new("bash", json!({}), cwd).header(), ["$ ..."]);
}

#[test]
fn shell_results_keep_the_last_lines_and_report_duration() {
    let cwd = Path::new("/tmp");
    let mut call = Call::new("bash", json!({"command": "seq 10"}), cwd);
    let text: Vec<String> = (1..=10).map(|n| n.to_string()).collect();
    let lines = call.result(&output(&text.join("\n")));
    assert!(lines.iter().any(|line| line.contains("... (5 earlier lines,")), "{lines:?}");
    assert!(lines.contains(&"10".to_string()) && !lines.contains(&"5".to_string()));
    assert_eq!(lines.last().unwrap(), "Took 1.5s");
    call.expanded = true;
    let lines = call.result(&output(&text.join("\n")));
    assert!(lines.contains(&"1".to_string()));
}

#[test]
fn shell_results_drop_the_truncation_footer_and_warn() {
    let cwd = Path::new("/tmp");
    let mut call = Call::new("bash", json!({"command": "big"}), cwd);
    let result = ToolResultView {
        content: vec![ContentBlock::text("tail line\n\n[Showing lines 1-2. Full output: /tmp/out.log]")],
        details: Some(json!({
            "truncation": {"truncated": true, "truncatedBy": "lines", "outputLines": 2, "totalLines": 9},
            "fullOutputPath": "/tmp/out.log"
        })),
        ..ToolResultView::default()
    };
    let lines = call.result(&result);
    assert!(!lines.iter().any(|line| line.contains("[Showing")), "{lines:?}");
    assert!(lines.contains(&"[Full output: /tmp/out.log. Truncated: showing 2 of 9 lines]".to_string()));
}

#[test]
fn read_headers_compact_context_files_and_skills() {
    let cwd = Path::new("/work");
    assert_eq!(
        Call::new("read", json!({"path": "src/a.rs", "offset": 5, "limit": 10}), cwd).header(),
        ["read src/a.rs:5-14"]
    );
    let header = Call::new("read", json!({"path": "AGENTS.md"}), cwd).header();
    assert!(header[0].starts_with("read resource AGENTS.md ("), "{header:?}");
    let header = Call::new("read", json!({"path": "/skills/deploy/SKILL.md"}), cwd).header();
    assert!(header[0].starts_with("[skill] deploy ("), "{header:?}");
}

#[test]
fn read_results_show_only_when_expanded_or_failed() {
    let cwd = Path::new("/work");
    let mut call = Call::new("read", json!({"path": "notes.txt"}), cwd);
    assert!(call.result(&output("one\ntwo")).is_empty());
    call.expanded = true;
    assert_eq!(call.result(&output("one\ntwo\n\n")), ["", "one", "two"]);
    let mut failed = Call::new("read", json!({"path": "missing.txt"}), cwd);
    let error = ToolResultView { is_error: true, ..output("ENOENT") };
    assert_eq!(failed.result(&error), ["", "ENOENT"]);
}

#[test]
fn search_tools_cut_output_and_warn_about_limits() {
    let cwd = Path::new("/work");
    assert_eq!(
        Call::new("grep", json!({"pattern": "fn", "glob": "*.rs", "limit": 5}), cwd).header(),
        ["grep /fn/ in . (*.rs) limit 5"]
    );
    assert_eq!(Call::new("find", json!({"pattern": "*.md", "path": "docs"}), cwd).header(), ["find *.md in docs"]);
    assert_eq!(Call::new("ls", json!({}), cwd).header(), ["ls ."]);
    let mut grep = Call::new("grep", json!({"pattern": "x"}), cwd);
    let lines: Vec<String> = (1..=20).map(|n| format!("a.rs:{n}: x")).collect();
    let result = ToolResultView {
        details: Some(json!({"matchLimitReached": 100, "linesTruncated": true})),
        ..output(&lines.join("\n"))
    };
    let rendered = grep.result(&result);
    assert_eq!(rendered.len(), 1 + 15 + 1 + 1, "{rendered:?}");
    assert!(rendered[16].starts_with("... (5 more lines,"));
    assert_eq!(rendered[17], "[Truncated: 100 matches limit, some lines truncated]");
}

#[test]
fn write_headers_show_content_and_invalid_args() {
    let cwd = Path::new("/work");
    let lines = Call::new("write", json!({"path": "a.txt", "content": "hello\nworld\n"}), cwd).header();
    assert_eq!(lines, ["write a.txt", "", "hello", "world"]);
    let lines = Call::new("write", json!({"path": "a.txt", "content": 3}), cwd).header();
    assert_eq!(lines, ["write a.txt", "", "[invalid content arg - expected string]"]);
}

#[test]
fn edit_previews_the_diff_and_reports_errors() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let args = json!({"path": "a.txt", "edits": [{"oldText": "two", "newText": "TWO"}]});
    let mut call = Call::new("edit", args, root.path());
    let header = call.header();
    assert!(header.iter().any(|line| line.contains("edit a.txt")), "{header:?}");
    assert!(header.iter().any(|line| line.contains("-2 two")) && header.iter().any(|line| line.contains("+2 TWO")));

    let missing = json!({"path": "nope.txt", "oldText": "a", "newText": "b"});
    let header = Call::new("edit", missing, root.path()).header();
    assert!(
        header.iter().any(|line| line.contains("Could not edit file: nope.txt. Error code: ENOENT.")),
        "{header:?}"
    );
}

#[test]
fn generic_headers_list_arguments() {
    let collapsed = format_tool_call_with_args("mcp_tool", &json!({"query": "rust", "limit": 3}), false);
    assert_eq!(pi_tui::strip_terminal_sequences(&collapsed), "mcp_tool query=\"rust\" limit=3");
    let expanded = format_tool_call_with_args("mcp_tool", &json!({"text": "a\nb"}), true);
    assert_eq!(pi_tui::strip_terminal_sequences(&expanded), "mcp_tool\n  text: a\n    b");
    let long = format_tool_call_with_args("t", &json!({"v": "x".repeat(200)}), false);
    assert!(pi_tui::strip_terminal_sequences(&long).ends_with("..."));
}

#[test]
fn mcp_results_preview_five_lines_and_name_the_full_output() {
    let cwd = Path::new("/work");
    let renderer = Rc::new(McpRenderer::from_tool_name("mcp__github__search").unwrap());
    let mut call = Call::with(renderer, json!({"q": "pi"}), cwd);
    assert_eq!(call.header(), ["github/search q=\"pi\""]);
    let lines: Vec<String> = (1..=8).map(|n| format!("row {n}")).collect();
    let result =
        ToolResultView { details: Some(json!({"fullOutputPath": "/tmp/mcp.txt"})), ..output(&lines.join("\n")) };
    let rendered = call.result(&result);
    assert_eq!(rendered[..6], ["", "row 1", "row 2", "row 3", "row 4", "row 5"], "{rendered:?}");
    assert!(rendered[6].starts_with("... (3 more lines,"), "{rendered:?}");
    assert_eq!(rendered[7], "Full output: /tmp/mcp.txt");
    call.expanded = true;
    assert_eq!(call.result(&result).len(), 9);
}

#[test]
fn codemode_lists_nested_calls_and_drops_the_script_header() {
    let cwd = Path::new("/work");
    let mut call = Call::new("codemode", json!({"code": "const a = 1;\nreturn a;"}), cwd);
    assert_eq!(call.header(), ["codemode", "const a = 1;", "return a;"]);
    let calls = json!([
        {"id": "c/1", "name": "read", "args": "{\"path\":\"a\"}", "status": "ok", "durationMs": 12.0},
        {"id": "c/2", "name": "models.complete", "args": "", "status": "ok", "durationMs": 1500.0, "cost": 0.02},
        {"id": "c/3", "name": "models.complete", "args": "", "status": "error", "cost": 0.0012, "error": "boom"}
    ]);
    let result = ToolResultView {
        content: vec![
            ContentBlock::text("Script completed\nWall time 0.5 seconds\nOutput:\n"),
            ContentBlock::text("done"),
        ],
        details: Some(json!({"calls": calls})),
        ..ToolResultView::default()
    };
    let rendered = call.result(&result);
    assert_eq!(
        rendered,
        [
            "",
            "✓ read {\"path\":\"a\"} 12ms",
            "✓ models.complete 1.5s $0.02",
            "✗ models.complete $0.0012",
            "Model calls: $0.02",
            "",
            "done"
        ]
    );
    call.expanded = true;
    assert!(call.result(&result).contains(&"    boom".to_string()));
}
