//! Port of upstream `test/system-prompt.test.ts`, plus snapshots with `SYSTEM.md` and
//! `APPEND_SYSTEM.md` from the resource loader.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::write;
use crate::config::ContextFile;
use crate::resources::source_info::create_synthetic_source_info;
use crate::resources::{ResourceLoader, ResourceLoaderOptions, Skill};
use crate::system_prompt::{
    BuildSystemPromptOptions, SystemPromptOptions, build_system_message, build_system_prompt,
    build_system_prompt_sections, diff_system_prompt_sections, render_system_prompt, sections_to_json,
};

fn test_skill() -> Skill {
    Skill {
        name: "test-skill".into(),
        description: "A test skill.".into(),
        file_path: PathBuf::from("/skills/test-skill/SKILL.md"),
        base_dir: PathBuf::from("/skills/test-skill"),
        source_info: create_synthetic_source_info("/skills/test-skill/SKILL.md", "test", None, None),
        disable_model_invocation: false,
        license: None,
        compatibility: None,
        metadata: Default::default(),
        allowed_tools: Vec::new(),
    }
}

fn options(tools: &[&str]) -> BuildSystemPromptOptions {
    let mut options = BuildSystemPromptOptions::new(Path::new("/tmp"));
    options.selected_tools = tools.iter().map(|tool| tool.to_string()).collect();
    options
}

fn render(options: &BuildSystemPromptOptions) -> String {
    render_system_prompt(options).unwrap()
}

#[test]
fn renders_structure_tools_and_rules() {
    let empty = render(&options(&[]));
    assert!(empty.starts_with("You are an expert coding assistant operating inside pi"));
    assert!(empty.contains("<tools>\n(none)\n"));
    assert!(empty.contains("Show file paths clearly"));
    assert!(empty.ends_with("<cwd>\n/tmp\n</cwd>"));

    let custom = BuildSystemPromptOptions { custom_prompt: Some("You are Exact.".into()), ..options(&[]) };
    assert!(render(&custom).starts_with("You are Exact.\n\n<cwd>"));
    let forced = BuildSystemPromptOptions { force_system_prompt: Some("exact".into()), ..options(&[]) };
    assert_eq!(render(&forced), "exact");
    assert_eq!(build_system_message(&forced, 5).unwrap().sections, None);

    let full = BuildSystemPromptOptions {
        custom_prompt: Some("You are Exact.".into()),
        append_system_prompt: "Additional instructions.".into(),
        context_files: vec![ContextFile { path: "/tmp/AGENTS.md".into(), content: "Project instructions.".into() }],
        ..options(&[])
    };
    let prompt = render(&full);
    assert!(prompt.contains("<addendum>\nAdditional instructions.\n</addendum>"));
    assert!(prompt.contains(
        "<project_context>\nProject-specific instructions and guidelines:\n\n<project_instructions path=\"/tmp/AGENTS.md\">"
    ));

    let snippets: HashMap<String, String> = [("read", "Read files"), ("dynamic_tool", "Run dynamic test behavior")]
        .map(|(name, text)| (name.to_string(), text.to_string()))
        .into();
    let with_snippets =
        BuildSystemPromptOptions { tool_snippets: snippets, ..options(&["read", "dynamic_tool", "other"]) };
    let prompt = render(&with_snippets);
    assert!(prompt.contains("- read: Read files\n- dynamic_tool: Run dynamic test behavior\n\nIn addition"));
    assert!(!prompt.contains("- other"));

    assert!(render(&options(&["powershell"])).contains("Use PowerShell for file operations"));
    assert!(render(&options(&["bash", "powershell"])).contains("Use bash or PowerShell for file operations"));
    assert!(render(&options(&["bash"])).contains("Use bash for file operations like ls, rg, find"));
    assert!(!render(&options(&["bash", "grep"])).contains("Use bash for file operations"));

    let guidelines = BuildSystemPromptOptions {
        prompt_guidelines: vec![
            "Use dynamic_tool for summaries.".into(),
            "  Use dynamic_tool for summaries.  ".into(),
            "  ".into(),
        ],
        ..options(&["read"])
    };
    assert_eq!(render(&guidelines).matches("- Use dynamic_tool for summaries.").count(), 1);
}

#[test]
fn skills_section_follows_the_reader_tool() {
    for custom in [None, Some("Custom system prompt".to_string())] {
        let prompt = render(&BuildSystemPromptOptions {
            custom_prompt: custom,
            skills: vec![test_skill()],
            ..options(&["bash"])
        });
        assert!(prompt.contains("<skills>\nThe following skills"));
        assert!(prompt.contains("<name>test-skill</name>"));
        assert!(prompt.contains("Use bash to load a skill's file"));
    }
    let prompt = render(&BuildSystemPromptOptions { skills: vec![test_skill()], ..options(&["write"]) });
    assert!(!prompt.contains("<available_skills>"));

    let build = |hidden: &[&str]| {
        let snippets = [("read", "Read files"), ("bash", "Run commands"), ("run", "Run a task")];
        let guidelines = [("read", "Use read for files."), ("run", "Prefer run.")];
        render(&BuildSystemPromptOptions {
            hidden_tools: hidden.iter().map(|name| name.to_string()).collect(),
            tool_snippets: snippets.iter().map(|(n, s)| (n.to_string(), s.to_string())).collect(),
            tool_guidelines: guidelines.iter().map(|(n, g)| (n.to_string(), vec![g.to_string()])).collect(),
            skills: vec![test_skill()],
            ..options(&["read", "bash", "run"])
        })
    };
    let hidden = build(&["read", "bash"]);
    assert!(hidden.contains("<tools>\n- run: Run a task\n"));
    assert!(!hidden.contains("- read: ") && !hidden.contains("Use read for files."));
    assert!(!hidden.contains("Use bash for file operations"));
    assert!(hidden.contains("- Prefer run."));
    assert!(hidden.contains("\nLoad a skill's file when the task matches its description."));
    assert!(build(&["read"]).contains("Use bash to load a skill's file"));
    assert!(build(&[]).contains("Use the read tool to load a skill's file"));
}

#[test]
fn custom_sections_validate_and_replace_in_place() {
    let mut options = options(&[]);
    options.sections = vec![("mcp_servers".into(), "- docs".into()), ("cwd".into(), "/elsewhere".into())];
    let sections = build_system_prompt_sections(&options).unwrap();
    let names: Vec<&str> = sections.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["preamble", "tools", "rules", "docs", "cwd", "mcp_servers"]);
    assert_eq!(sections[4].1, "<cwd>\n/elsewhere\n</cwd>");
    for bad in ["preamble", "Bad", "1x", ""] {
        options.sections = vec![(bad.into(), "x".into())];
        assert!(build_system_prompt_sections(&options).is_err(), "{bad}");
    }
}

#[test]
fn builds_system_messages_and_diffs_sections() {
    let first = options(&[]);
    let message = build_system_message(&first, 7).unwrap();
    assert_eq!(message.timestamp, 7);
    assert_eq!(message.content.text(), "");
    let sections = message.sections.clone().unwrap();
    assert_eq!(sections.keys().collect::<Vec<_>>(), ["preamble", "tools", "rules", "docs", "cwd"]);
    assert_eq!(pi_ai::transcript::system_message_text(&message), render(&first));

    let current = build_system_prompt_sections(&first).unwrap();
    assert_eq!(diff_system_prompt_sections(&sections_to_json(&current), &current), None);
    let next = BuildSystemPromptOptions { append_system_prompt: "More.".into(), cwd: "/w".into(), ..options(&[]) };
    let mut previous = sections_to_json(&current);
    previous.insert("stale".into(), "<stale>\nx\n</stale>".into());
    let patch = diff_system_prompt_sections(&previous, &build_system_prompt_sections(&next).unwrap()).unwrap();
    assert_eq!(patch.keys().collect::<Vec<_>>(), ["addendum", "cwd", "stale"]);
    assert_eq!(patch["stale"], serde_json::Value::Null);
    assert_eq!(patch["cwd"], "<cwd>\n/w\n</cwd>");
}

#[test]
fn legacy_options_keep_builtin_tool_snippets() {
    let tools = ["read", "bash"].map(String::from).to_vec();
    let context = [ContextFile { path: PathBuf::from("/repo/AGENTS.md"), content: "Use tabs.".into() }];
    let prompt = build_system_prompt(&SystemPromptOptions {
        selected_tools: &tools,
        context_files: &context,
        append_prompt: Some("Extra."),
        ..SystemPromptOptions::new(Path::new("/repo"))
    });
    assert!(prompt.contains("<tools>\n- read: "));
    assert!(prompt.contains("- Use bash for file operations like ls, rg, find"));
    assert!(prompt.contains("<addendum>\nExtra.\n</addendum>"));
    assert!(prompt.contains("<project_instructions path=\"/repo/AGENTS.md\">\nUse tabs.\n</project_instructions>"));
    assert!(prompt.ends_with("<cwd>\n/repo\n</cwd>"));
    let custom = SystemPromptOptions { custom_prompt: Some("Custom."), ..SystemPromptOptions::new(Path::new("/w")) };
    assert_eq!(build_system_prompt(&custom), "Custom.\n\n<cwd>\n/w\n</cwd>");
}

fn prompt_from_loader(cwd: &Path, agent: &Path, home: &Path, trusted: bool) -> String {
    let mut options = ResourceLoaderOptions::new(cwd, agent);
    options.home = home.to_path_buf();
    options.project_trusted = trusted;
    let mut loader = ResourceLoader::new(options);
    loader.reload();
    let mut prompt = BuildSystemPromptOptions::new(cwd).with_resources(loader.resources());
    prompt.selected_tools = vec!["read".into()];
    prompt.tool_snippets.insert("read".into(), "Read file contents".into());
    render(&prompt)
}

#[test]
fn snapshots_with_system_and_append_system_files() {
    let temp = tempfile::tempdir().unwrap();
    let (agent, cwd, home) = (temp.path().join("agent"), temp.path().join("p"), temp.path().join("home"));
    write(&agent.join("APPEND_SYSTEM.md"), "Global addendum.");
    write(&agent.join("skills/lint/SKILL.md"), "---\nname: lint\ndescription: Lint & fix.\n---\nRun it.");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_text = cwd.display().to_string().replace('\\', "/");
    let skill_path = agent.join("skills").join("lint").join("SKILL.md").display().to_string();
    let skills = format!(
        "<skills>\nThe following skills provide specialized instructions for specific tasks.\nUse the read tool to load a skill's file when the task matches its description.\nWhen a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.\n\n<available_skills>\n  <skill>\n    <name>lint</name>\n    <description>Lint &amp; fix.</description>\n    <location>{skill_path}</location>\n  </skill>\n</available_skills>\n</skills>"
    );
    let default_prompt = prompt_from_loader(&cwd, &agent, &home, true);
    let head = "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\n<tools>\n- read: Read file contents\n\nIn addition to the tools above, you may have access to other custom tools depending on the project.\n</tools>\n\n<rules>\n- Be concise in your responses\n- Show file paths clearly when working with files\n</rules>\n\n<docs>\n";
    assert!(default_prompt.starts_with(head), "{default_prompt}");
    let tail = format!("</docs>\n\n<addendum>\nGlobal addendum.\n</addendum>\n\n{skills}\n\n<cwd>\n{cwd_text}\n</cwd>");
    assert!(default_prompt.ends_with(&tail), "{default_prompt}");

    write(&agent.join("SYSTEM.md"), "Global system.");
    write(&cwd.join(".pi/SYSTEM.md"), "Project system.");
    write(&cwd.join(".pi/APPEND_SYSTEM.md"), "Project addendum.");
    let expected = |system: &str, addendum: &str| {
        format!("{system}\n\n<addendum>\n{addendum}\n</addendum>\n\n{skills}\n\n<cwd>\n{cwd_text}\n</cwd>")
    };
    assert_eq!(prompt_from_loader(&cwd, &agent, &home, true), expected("Project system.", "Project addendum."));
    assert_eq!(prompt_from_loader(&cwd, &agent, &home, false), expected("Global system.", "Global addendum."));
}
