//! Tool factories and built-in tool metadata, ported from upstream
//! `builtin-tool-strict-mode.test.ts` and the read tests of `tools.test.ts`.

use std::sync::Arc;

use base64::Engine;
use pi_agent::{AgentTool, ToolContext};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{noop, text};
use crate::settings::SettingsManager;
use crate::tools::{
    ALL_TOOLS, OutputAccumulator, ReadTool, ReadToolOptions, ToolsOptions, create_all_tools, create_coding_tools,
    create_powershell_tool, create_read_only_tools, create_tool, create_tools_with_options, tool_info,
};

const PNG_1X1: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg==";

fn names(tools: &[pi_agent::SharedTool]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name()).collect()
}

#[test]
fn factories_build_the_upstream_tool_sets() {
    let dir = tempfile::tempdir().unwrap();
    let options = ToolsOptions::default();
    assert_eq!(names(&create_coding_tools(dir.path(), &options)), ["read", "bash", "edit", "write"]);
    assert_eq!(names(&create_read_only_tools(dir.path(), &options)), ["read", "grep", "find", "ls"]);
    let all = create_all_tools(dir.path(), &options);
    let mut expected: Vec<&str> = ALL_TOOLS.to_vec();
    expected.sort_unstable();
    assert_eq!(all.keys().copied().collect::<Vec<_>>(), expected);
    assert_eq!(create_tool("powershell", dir.path()).is_some(), cfg!(windows));
    assert!(create_tools_with_options(&["read", "nope"], dir.path(), &options).is_err());
}

#[test]
fn file_and_shell_tools_prefer_strict_sampling() {
    let dir = tempfile::tempdir().unwrap();
    let all = create_all_tools(dir.path(), &ToolsOptions::default());
    let powershell = create_powershell_tool(dir.path(), Default::default());
    let strict = ["read", "bash", "edit", "write"].iter().map(|name| all[name].clone()).chain([powershell]);
    for tool in strict {
        assert_eq!(tool.constrained_sampling(), Some(json!({ "type": "json_schema", "strict": "prefer" })));
        assert_eq!(tool.declaration().constrained_sampling, tool.constrained_sampling());
    }
    for name in ["grep", "find", "ls"] {
        assert_eq!(all[name].constrained_sampling(), None, "{name}");
    }
    // Strictness is a provider-side conversion, not a change to the execution schema.
    assert_eq!(all["read"].parameters()["required"], json!(["path"]));
    assert_eq!(all["bash"].parameters()["required"], json!(["command"]));
    assert!(all["read"].output_schema().is_some() && all["bash"].output_schema().is_some());
}

#[test]
fn tools_carry_their_prompt_snippets_and_guidelines() {
    let dir = tempfile::tempdir().unwrap();
    for (name, tool) in create_all_tools(dir.path(), &ToolsOptions::default()) {
        let info = tool_info(name).unwrap();
        assert_eq!(tool.prompt_snippet(), Some(info.snippet), "{name}");
        assert_eq!(tool.prompt_guidelines(), info.guidelines, "{name}");
    }
    assert_eq!(tool_info("powershell").unwrap().snippet, "Execute PowerShell commands");
}

#[test]
fn options_come_from_settings() {
    let settings = SettingsManager::in_memory(json!({
        "shellPath": "/opt/bash",
        "shellCommandPrefix": "shopt -s expand_aliases",
        "images": { "autoResize": false }
    }));
    let options = ToolsOptions::from_settings(&settings);
    assert!(!options.read.auto_resize_images);
    assert_eq!(options.bash.shell_path.as_deref(), Some(std::path::Path::new("/opt/bash")));
    assert_eq!(options.bash.command_prefix.as_deref(), Some("shopt -s expand_aliases"));
    assert!(options.bash.expose_session_environment);
    let defaults = ToolsOptions::from_settings(&SettingsManager::in_memory(json!({})));
    assert!(defaults.read.auto_resize_images && defaults.bash.shell_path.is_none());
}

#[tokio::test]
async fn read_returns_text_and_images_as_structured_content() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "line one\nline two").unwrap();
    let png = base64::engine::general_purpose::STANDARD.decode(PNG_1X1).unwrap();
    std::fs::write(dir.path().join("image.txt"), png).unwrap();
    let tool = create_tool("read", dir.path()).unwrap();

    let result = tool.execute("r1", json!({ "path": "notes.txt" }), CancellationToken::new(), noop()).await.unwrap();
    assert_eq!(result.structured_content, Some(json!("line one\nline two")));

    let result = tool.execute("r2", json!({ "path": "image.txt" }), CancellationToken::new(), noop()).await.unwrap();
    let Some(pi_ai::ContentBlock::Image { data, mime_type }) = result.content.get(1) else {
        panic!("expected an image block");
    };
    let expected = json!({ "type": "image", "data": data, "mimeType": mime_type, "note": text(&result) });
    assert_eq!(result.structured_content, Some(expected));
    assert_eq!(text(&result), "Read image file [image/png]");
}

#[tokio::test]
async fn read_uses_the_model_of_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let png = base64::engine::general_purpose::STANDARD.decode(PNG_1X1).unwrap();
    std::fs::write(dir.path().join("image.png"), png).unwrap();
    let mut text_only = pi_ai::providers::faux::FauxProvider::model();
    text_only.input = vec!["text".into()];
    let vision = pi_ai::providers::faux::FauxProvider::model();
    let note = "[Current model does not support images. The image will be omitted from this request.]";

    let tool = ReadTool::with_options(dir.path().to_path_buf(), ReadToolOptions::default());
    let args = json!({ "path": "image.png" });
    let context = ToolContext::new("r1").with_model(Some(Arc::new(text_only.clone())));
    let result = tool.execute_with_context(&context, args.clone(), CancellationToken::new(), noop()).await.unwrap();
    assert!(text(&result).ends_with(note), "{}", text(&result));

    // A model of the call overrides the model the tool was built for.
    let for_text_model = ReadTool::for_model(dir.path().to_path_buf(), &text_only, true);
    let context = ToolContext::new("r2").with_model(Some(Arc::new(vision)));
    let result =
        for_text_model.execute_with_context(&context, args.clone(), CancellationToken::new(), noop()).await.unwrap();
    assert!(!text(&result).contains(note));
    let result = for_text_model.execute("r3", args, CancellationToken::new(), noop()).await.unwrap();
    assert!(text(&result).ends_with(note));
}

#[test]
fn full_output_keeps_the_head_and_tail_at_character_boundaries() {
    let mut acc = OutputAccumulator::with_limits("pi-test", 10, 16);
    acc.append("é".repeat(40).as_bytes());
    acc.finish();
    let path = acc.snapshot(true).full_output_path.unwrap();
    acc.close_temp_file();
    let full = acc.read_full_output(11);
    assert!(full.truncated);
    assert_eq!(full.content, "éé\n\n[... 69 bytes omitted ...]\n\nééé");
    assert_eq!(acc.read_full_output(1000).content, "é".repeat(40));
    let _ = std::fs::remove_file(path);

    let mut small = OutputAccumulator::new("pi-test");
    small.append(b"short");
    small.finish();
    let full = small.read_full_output(1000);
    assert_eq!((full.content.as_str(), full.truncated), ("short", false));
}
