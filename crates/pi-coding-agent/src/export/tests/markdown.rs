use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::export::markdown::{details, fenced, inline_code, quoted};
use crate::export::session_file::parse_session_entries;
use crate::export::{MAX_RESULT_LINES, SessionSnapshot, generate_markdown, is_markdown_path, write_markdown};
use crate::session::{NewSessionOptions, SessionManager};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/export_markdown.jsonl")
}

fn markdown() -> String {
    generate_markdown(&SessionSnapshot::load(&fixture()).unwrap())
}

/// Assert that every part appears in `text`, in this order.
fn assert_in_order(text: &str, parts: &[&str]) {
    let mut from = 0;
    for part in parts {
        let found = text[from..].find(part).unwrap_or_else(|| panic!("missing {part:?} after byte {from}:\n{text}"));
        from += found + part.len();
    }
}

#[test]
fn header_has_title_and_metadata() {
    let text = markdown();
    let expected = "# Markdown fixture\n\n\
        - **Date:** 2026-03-04 05:06:07 UTC\n\
        - **Models:** `anthropic/claude-sonnet-4-5`, `openai/gpt-5.5`\n\
        - **Working directory:** `~/work/demo`\n\
        - **Tokens:** 3,280 (input 2,200, output 80, cache read 1,000)\n\
        - **Cost:** $0.005\n\n---\n\n## User\n\n";
    assert!(text.starts_with(expected), "{text}");
    assert!(text.ends_with("> Remember the tests.\n"), "{text}");
}

#[test]
fn title_falls_back_to_first_prompt() {
    let mut snapshot = SessionSnapshot::load(&fixture()).unwrap();
    snapshot.entries.retain(|entry| entry["type"] != "session_info");
    snapshot.leaf_id = Some("b15".into());
    assert!(generate_markdown(&snapshot).starts_with("# List the files, then show `README.md`.\n"));
    let long = format!("{}\nsecond line", "word ".repeat(30));
    snapshot.entries[1]["message"]["content"] = Value::String(long);
    assert!(generate_markdown(&snapshot).starts_with(&format!("# {}...\n", "word ".repeat(16).trim_end())));
}

#[test]
fn turns_show_text_thinking_and_tool_calls() {
    let text = markdown();
    assert_in_order(
        &text,
        &[
            "## User\n\nList the files, then show `README.md`.\n\n*[Image: image/png]*\n\n",
            "## Assistant\n\n<details>\n<summary>Thinking</summary>\n\nI should run ls first.\n\n</details>\n\n",
            "Let me look.\n\n### Tool call: `bash`\n\n```json\n{\n  \"command\": \"ls\"\n}\n```\n\n**Result:**\n\n```\nfile-1.txt\n",
            "### Tool call: `read`\n\n```json\n{\n  \"path\": \"README.md\"\n}\n```\n\n**Result:**\n\n",
            "### Tool call: `grep`\n\n```json\n{\n  \"pattern\": \"a`b\"\n}\n```\n\n**Result (error):**\n\n```\nInvalid pattern\n```",
            "*Switched to model `openai/gpt-5.5`*\n\n## Assistant\n\nFound 250 files.\n\n",
            "## User\n\nThanks!\n\n## Assistant\n\n*Aborted*\n\n",
            "## Bash\n\n```\n$ cat notes.txt\nno such file\n```\n\n*Exit code 1*",
        ],
    );
    assert!(!text.contains("\x1b"), "ANSI codes are stripped");
    assert!(!text.contains("### Tool result"), "every result sits under its call");
}

#[test]
fn long_tool_results_are_truncated() {
    let text = markdown();
    let kept = format!("file-{MAX_RESULT_LINES}.txt\n```\n\n*Output truncated: showing 200 of 250 lines.*");
    assert!(text.contains(&kept), "{text}");
    assert!(!text.contains(&format!("file-{}.txt", MAX_RESULT_LINES + 1)));
}

#[test]
fn images_become_placeholders() {
    let text = markdown();
    assert!(text.contains("*[Image: image/png]*") && text.contains("*[Image: image/jpeg]*"));
    assert!(!text.contains("iVBORw0KGgo"), "image data is left out");
}

#[test]
fn summaries_and_custom_messages_are_quoted() {
    assert_in_order(
        &markdown(),
        &[
            "> **Compaction:** compacted from 54,321 tokens.\n>\n> The user listed the files.\n> The README was read.\n\n",
            "> **Branch summary:**\n>\n> Tried another way and stopped.\n\n",
            "> **note:**\n>\n> Remember the tests.\n",
        ],
    );
}

#[test]
fn fences_outgrow_backtick_runs() {
    let text = markdown();
    let read = "`````\n# Demo\n\n```rust\nfn main() {}\n```\n\nA run of four: ````\n`````\n\n*[Image: image/jpeg]*";
    assert!(text.contains(read), "{text}");
    assert_eq!(fenced("plain", "json"), "```json\nplain\n```");
    assert_eq!(fenced("a ``` b", ""), "````\na ``` b\n````");
    assert_eq!(inline_code("a`b"), "``a`b``");
    assert_eq!(inline_code("`x"), "`` `x ``");
    assert_eq!(quoted("a\n\nb\n"), "> a\n>\n> b");
    assert_eq!(details("S", " x </details> "), "<details>\n<summary>S</summary>\n\nx &lt;/details>\n\n</details>");
}

#[test]
fn skill_prompts_collapse_the_skill() {
    let prompt =
        "<skill name=\"review\" location=\"/skills/review/SKILL.md\">\nCheck the diff.\n</skill>\n\nLook at main.rs";
    let entries = vec![
        json!({"type": "session", "version": 3, "id": "s1", "timestamp": "t", "cwd": "/w"}),
        json!({"type": "message", "id": "m1", "parentId": null, "message": {"role": "user", "content": prompt}}),
    ];
    let text = generate_markdown(&SessionSnapshot::from_values("s1.jsonl", entries));
    assert!(text.starts_with("# Look at main.rs\n"), "{text}");
    let skill =
        "## User\n\n<details>\n<summary>Skill: review</summary>\n\nCheck the diff.\n\n</details>\n\nLook at main.rs\n";
    assert!(text.contains(skill), "{text}");
}

#[test]
fn follows_the_current_branch() {
    let text = markdown();
    assert!(!text.contains("An abandoned prompt"), "{text}");
    let entries = parse_session_entries(&std::fs::read_to_string(fixture()).unwrap());
    let entries = entries.into_iter().filter_map(|entry| entry.as_object().cloned()).collect();
    let mut manager =
        SessionManager::in_memory_with(Path::new("/work"), NewSessionOptions::default(), entries).unwrap();
    manager.branch("b3").unwrap();
    let text = generate_markdown(&SessionSnapshot::from_manager(&manager));
    assert!(text.starts_with("# Markdown fixture\n"), "{text}");
    assert!(text.contains("### Tool call: `grep`") && !text.contains("**Result"), "{text}");
    assert!(!text.contains("Found 250 files."));
}

#[test]
fn markdown_paths_and_writing() {
    assert!(is_markdown_path("out.md") && is_markdown_path("dir/Notes.MD") && is_markdown_path(".md"));
    assert!(!is_markdown_path("out.html") && !is_markdown_path("out.mdx") && !is_markdown_path("md"));
    assert!(!is_markdown_path("é"));
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("session.md");
    let snapshot = SessionSnapshot::load(&fixture()).unwrap();
    assert_eq!(write_markdown(&snapshot, out.to_str().unwrap()).unwrap(), out);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), generate_markdown(&snapshot));
}
