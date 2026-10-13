//! Upstream `prompt-to-pi-message`, `pi-messages`, `pi-tools`, `slash-commands`,
//! `pi-commands`, `merge-commands`, `auth-methods-terminal-auth-meta`, and
//! `pi-enable-embed-context-flag` tests, plus the bash translation.

use agent_client_protocol_schema::{AvailableCommand, ContentBlock};
use serde_json::{Value, json};

use crate::acp::agent::embedded_context_enabled;
use crate::acp::auth::{PI_SETUP_METHOD_ID, TerminalLaunch, auth_methods_json};
use crate::acp::bash::{bash_command, bash_exit_code, bash_output_delta, bash_result_text};
use crate::acp::commands::{
    FileSlashCommand, commands_from_pi, expand_slash_command, load_slash_commands, merge_commands, parse_command_args,
    substitute_args, to_available_commands,
};
use crate::acp::translate::{
    normalize_pi_assistant_text, normalize_pi_message_text, prompt_to_pi_message, prompt_to_session_title,
    tool_result_to_text,
};

fn blocks(value: Value) -> Vec<ContentBlock> {
    serde_json::from_value(value).unwrap()
}

fn base64(text: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(text)
}

#[test]
fn prompt_concatenates_text_and_resource_links() {
    let (message, images) = prompt_to_pi_message(&blocks(json!([
        {"type": "text", "text": "Hello"},
        {"type": "resource_link", "uri": "file:///tmp/foo.txt", "name": "foo"},
        {"type": "text", "text": " world"},
    ])));
    assert_eq!(message, "Hello\n[Context] file:///tmp/foo.txt world");
    assert!(images.is_empty());
}

#[test]
fn session_titles_normalize_text_and_stop_at_80_characters() {
    let title = prompt_to_session_title(&blocks(json!([
        {"type": "text", "text": "  Fix\n\tthe "},
        {"type": "resource_link", "uri": "file:///tmp/foo.txt", "name": "foo"},
        {"type": "text", "text": " thread  title "},
    ])));
    assert_eq!(title.as_deref(), Some("Fix the thread title"));
    let long = prompt_to_session_title(&blocks(json!([{"type": "text", "text": format!("  {}  ", "😀".repeat(81))}])));
    assert_eq!(long, Some("😀".repeat(80)));
    let none = prompt_to_session_title(&blocks(json!([{"type": "resource_link", "uri": "file:///x", "name": "x"}])));
    assert_eq!(none, None);
}

#[test]
fn embedded_resources_and_audio_become_markers() {
    let (text, _) = prompt_to_pi_message(&blocks(json!([
        {"type": "resource", "resource": {"uri": "file:///tmp/a.txt", "mimeType": "text/plain", "text": "hi"}}
    ])));
    assert_eq!(text, "\n[Embedded Context] file:///tmp/a.txt (text/plain)\nhi");
    let (blob, _) = prompt_to_pi_message(&blocks(json!([
        {"type": "resource", "resource": {"uri": "file:///tmp/a.bin", "mimeType": "application/octet-stream",
            "blob": base64("xyz")}}
    ])));
    assert_eq!(blob, "\n[Embedded Context] file:///tmp/a.bin (application/octet-stream, 3 bytes)");
    let (audio, images) =
        prompt_to_pi_message(&blocks(json!([{"type": "audio", "mimeType": "audio/wav", "data": base64("abc")}])));
    assert_eq!(audio, "\n[Audio] (audio/wav, 3 bytes) not supported by pi-acp");
    assert!(images.is_empty());
}

#[test]
fn images_become_pi_image_content() {
    let data = base64("abc");
    let (message, images) = prompt_to_pi_message(&blocks(json!([
        {"type": "text", "text": "see"},
        {"type": "image", "mimeType": "image/png", "data": data, "uri": "img-1"},
    ])));
    assert_eq!(message, "see");
    assert_eq!(images, vec![pi_ai::ContentBlock::Image { data, mime_type: "image/png".into() }]);
}

#[test]
fn normalizes_pi_message_text() {
    assert_eq!(normalize_pi_message_text(&json!("hello")), "hello");
    let mixed = json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}, {"type": "not_text", "x": 1}]);
    assert_eq!(normalize_pi_message_text(&mixed), "ab");
    let assistant =
        json!([{"type": "text", "text": "hi"}, {"type": "thinking", "text": "..."}, {"type": "text", "text": "!"}]);
    assert_eq!(normalize_pi_assistant_text(&assistant), "hi!");
    assert_eq!(normalize_pi_assistant_text(&json!("plain")), "");
}

#[test]
fn tool_results_become_text() {
    let content = json!({"content": [{"type": "text", "text": "hello"}, {"type": "text", "text": " world"}]});
    assert_eq!(tool_result_to_text(&content), "hello world");
    let diff =
        json!({"content": [{"type": "text", "text": "Successfully replaced"}], "details": {"diff": "--- a\n+++ b\n"}});
    assert_eq!(tool_result_to_text(&diff), "--- a\n+++ b\n");
    assert!(tool_result_to_text(&json!({"a": 1})).contains("\"a\": 1"));
    let bash = tool_result_to_text(&json!({"details": {"stdout": "ok\n", "stderr": "warn\n", "exitCode": 0}}));
    assert_eq!(bash, "ok\n\n\nstderr:\nwarn\n\n\nexit code: 0");
    assert_eq!(tool_result_to_text(&Value::Null), "");
}

#[test]
fn parses_quoted_command_arguments() {
    assert_eq!(parse_command_args("a b"), ["a", "b"]);
    assert_eq!(parse_command_args("'a b' c"), ["a b", "c"]);
    assert_eq!(parse_command_args("\"a b\" c"), ["a b", "c"]);
    assert_eq!(parse_command_args("  x\ty  "), ["x", "y"]);
}

#[test]
fn substitutes_positional_and_all_arguments() {
    let args = vec!["one".to_string(), "two".to_string()];
    assert_eq!(substitute_args("x=$1 y=$2 all=$@", &args).trim(), "x=one y=two all=one two");
    assert_eq!(substitute_args("$3", &["one".into()]).trim(), "");
    assert_eq!(substitute_args("cost $ 5", &args), "cost $ 5");
}

fn command(name: &str, description: &str, content: &str) -> FileSlashCommand {
    FileSlashCommand {
        name: name.into(),
        description: description.into(),
        content: content.into(),
        source: "(user)".into(),
    }
}

#[test]
fn expands_known_slash_commands_only() {
    let commands = vec![command("hello", "(user)", "Say hi to $1")];
    assert_eq!(expand_slash_command("/hello world", &commands), "Say hi to world");
    assert_eq!(expand_slash_command("/unknown world", &commands), "/unknown world");
    assert_eq!(expand_slash_command("not a command", &commands), "not a command");
}

#[test]
fn available_commands_keep_the_first_of_each_name() {
    let commands = vec![command("x", "first", "1"), command("x", "second", "2")];
    let available = serde_json::to_value(to_available_commands(&commands)).unwrap();
    assert_eq!(available, json!([{"name": "x", "description": "first"}]));
    let merged = merge_commands(
        vec![AvailableCommand::new("a", ""), AvailableCommand::new("b", "")],
        vec![AvailableCommand::new("b", "other"), AvailableCommand::new("c", "")],
    );
    let names: Vec<&str> = merged.iter().map(|command| command.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "c"]);
    assert_eq!(merged[1].description, "");
}

#[test]
fn loads_prompt_files_with_frontmatter_and_sources() {
    let root = tempfile::tempdir().unwrap();
    let (cwd, agent_dir) = (root.path().join("work"), root.path().join("agent"));
    std::fs::create_dir_all(agent_dir.join("prompts/team")).unwrap();
    std::fs::create_dir_all(cwd.join(".pi/prompts")).unwrap();
    std::fs::write(agent_dir.join("prompts/review.md"), "---\ndescription: Review code\n---\nReview $1").unwrap();
    std::fs::write(agent_dir.join("prompts/team/plan.md"), format!("{}\nmore", "p".repeat(70))).unwrap();
    std::fs::write(cwd.join(".pi/prompts/fix.md"), "Fix it").unwrap();
    let commands = load_slash_commands(&cwd, &agent_dir);
    let summary: Vec<(String, String)> =
        commands.iter().map(|command| (command.name.clone(), command.description.clone())).collect();
    assert_eq!(
        summary,
        [
            ("review".to_string(), "Review code (user)".to_string()),
            ("plan".to_string(), format!("{}... (user:team)", "p".repeat(60))),
            ("fix".to_string(), "Fix it (project)".to_string()),
        ]
    );
    assert_eq!(commands[0].content, "Review $1");
}

#[test]
fn pi_commands_hide_extensions_and_optionally_skills() {
    let data = json!({"commands": [
        {"name": "x", "description": "X", "source": "extension"},
        {"name": "skill:foo", "description": "Foo", "source": "skill", "location": "user"},
        {"name": "y", "source": "prompt", "location": "project"},
    ]});
    let to_json = |commands: Vec<AvailableCommand>| serde_json::to_value(commands).unwrap();
    assert_eq!(
        to_json(commands_from_pi(&data, true, false)),
        json!([{"name": "skill:foo", "description": "Foo"}, {"name": "y", "description": "(prompt:project)"}])
    );
    assert_eq!(
        to_json(commands_from_pi(&data, true, true)),
        json!([{"name": "x", "description": "X"}, {"name": "skill:foo", "description": "Foo"},
            {"name": "y", "description": "(prompt:project)"}])
    );
    assert_eq!(
        to_json(commands_from_pi(&data, false, false)),
        json!([{"name": "y", "description": "(prompt:project)"}])
    );
}

#[test]
fn auth_methods_carry_terminal_auth_metadata_when_supported() {
    let launch = TerminalLaunch { command: "/usr/bin/pi".into(), args: vec!["--terminal-login".into()] };
    let with_meta = auth_methods_json(true, &launch);
    let method = &with_meta[0];
    assert_eq!(method["id"], PI_SETUP_METHOD_ID);
    assert_eq!(method["type"], "terminal");
    assert_eq!(method["args"], json!(["--terminal-login"]));
    assert_eq!(
        method["_meta"]["terminal-auth"],
        json!({"command": "/usr/bin/pi", "args": ["--terminal-login"], "label": "Launch pi"})
    );
    let without = auth_methods_json(false, &launch);
    assert!(without[0].get("_meta").is_none());
}

#[test]
fn embedded_context_needs_the_exact_value_true() {
    assert!(!embedded_context_enabled(None));
    assert!(!embedded_context_enabled(Some("false")));
    assert!(!embedded_context_enabled(Some("1")));
    assert!(embedded_context_enabled(Some("true")));
}

#[test]
fn bash_translation_finds_commands_output_and_exit_codes() {
    assert_eq!(bash_command(&json!({"command": "ls"})).as_deref(), Some("ls"));
    assert_eq!(bash_command(&json!({"args": {"cmd": "pwd"}})).as_deref(), Some("pwd"));
    assert_eq!(bash_command(&json!({"command": "  "})), None);
    let result = json!({"content": [{"type": "text", "text": "out"}], "details": {"exitCode": 2}});
    assert_eq!((bash_result_text(&result), bash_exit_code(&result, false)), ("out".to_string(), 2));
    let split = json!({"details": {"stdout": "a", "stderr": "b"}});
    assert_eq!(bash_result_text(&split), "a\nb");
    assert_eq!((bash_exit_code(&json!({}), true), bash_exit_code(&json!({}), false)), (1, 0));
    assert_eq!(bash_output_delta("abc", "abcdef"), "def");
    assert_eq!(bash_output_delta("xyz", "abc"), "abc");
}
