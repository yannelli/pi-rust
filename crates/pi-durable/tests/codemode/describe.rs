//! Port of upstream `test/codemode-describe.test.ts`: the code mode tool's description and the `codemode` section.

use pi_codemode::CODEMODE_SOURCE_GRAMMAR;
use pi_durable::codemode::{codemode_description, codemode_section, create_codemode_extension};

use super::*;

fn described(name: &str, description: &str) -> Arc<ToolRegistration> {
    let parameters = json!({
        "type": "object",
        "properties": { "path": { "type": "string", "description": "File path" } },
        "required": ["path"],
    });
    let tool = define_tool(name, description, parameters, |_, _, _| async {
        Ok(ToolExecutionResult { output: Some(Vec::new()), ..ToolExecutionResult::default() })
    });
    Arc::new(tool)
}

fn github() -> CodemodeNamespace {
    CodemodeNamespace::new("github", "mcp__github__").with_description("GitHub MCP server")
}

#[test]
fn keeps_the_tool_description_the_same_for_every_conversation() {
    let text = codemode_description(false);
    assert!(text.contains("`await tools.<name>({ ...args })`"));
    assert!(text.contains("searchTools(query"));
    assert!(text.contains("The `codemode` section of the system prompt lists the tools scripts can call"));
    assert!(!text.contains("`models`"));
    assert!(codemode_description(true).contains("`models`"));
}

#[test]
fn lists_callable_tools_as_declarations_grouped_by_namespace_with_namespace_descriptions() {
    let tools = [described("read", "Read a file."), described("mcp__github__list_issues", "List issues.")];
    let section = codemode_section(&tools, &[github()], 3000).unwrap();
    assert!(section.contains("### `read`\nRead a file."), "{section}");
    assert!(section.contains("read(args: {"), "{section}");
    assert!(section.contains("## github\nGitHub MCP server"), "{section}");
    // Tools before namespaces; a namespace's tools after its heading.
    let at = |text: &str| section.find(text).unwrap();
    assert!(at("### `read`") < at("## github"));
    assert!(at("## github") < at("### `mcp__github__list_issues`"));
    assert!(section.contains("Promise<string>"), "{section}");
}

#[test]
fn lists_what_fits_the_budget_every_namespace_before_any_is_complete_and_says_what_is_left_out() {
    let github_tools: Vec<_> =
        (0..30).map(|index| described(&format!("mcp__github__tool{index}"), &"x".repeat(200))).collect();
    let mut tools = vec![described("read", "Read a file.")];
    tools.extend(github_tools.iter().cloned());
    let section = codemode_section(&tools, &[github()], 400).unwrap();
    assert!(section.contains("### `read`"), "{section}");
    assert!(section.contains("## github (some tools not listed)"), "{section}");
    assert!(!section.contains("tool29"), "{section}");
    assert!(codemode_section(&github_tools, &[github()], 10).unwrap().contains("## github (tools not listed)"));
    assert_eq!(codemode_section(&[], &[github()], 3000), None);
}

#[test]
fn uses_the_longest_matching_namespace_prefix() {
    let namespaces = [CodemodeNamespace::new("git", "mcp__git"), github()];
    let section = codemode_section(&[described("mcp__github__x", "x")], &namespaces, 3000).unwrap();
    assert!(section.contains("## github"), "{section}");
    assert!(!section.contains("## git\n"), "{section}");
}

#[tokio::test]
async fn renders_the_section_from_the_conversation_s_callable_tools_into_the_system_prompt() {
    let setup = chat_setup(Default::default());
    let mut options = CodemodeToolOptions::new(new_sandbox());
    options.namespaces = vec![github()];
    setup.registry.install(create_codemode_extension(options)).unwrap();
    let issues = (*described("mcp__github__list_issues", "List issues.")).clone().with_callers(vec![ToolCaller::Tools]);
    let get_issue = (*described("mcp__github__get_issue", "Get an issue.")).clone().with_deferred();
    let tools = Extension::new("tools").tool(described("read", "Read a file.")).tool(issues).tool(get_issue);
    setup.registry.install(tools).unwrap();
    setup.faux.set_responses([answer("hi")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let id = submit(&root, "go").await;
    wait(&harness, id).await;
    let entries = all_entries(&root).await;
    let system = entries.iter().find(|entry| entry.kind == "pi.system").unwrap();
    let Some(Message::System(message)) = system.model.as_ref().and_then(|model| model.first()) else { panic!() };
    let sections = message.sections.clone().unwrap();
    let codemode = sections["codemode"].as_str().unwrap();
    assert!(codemode.contains("### `read`"), "{codemode}");
    assert!(codemode.contains("### `mcp__github__list_issues`"), "{codemode}");
    // Deferred tools are found with searchTools(), not listed.
    assert!(!codemode.contains("mcp__github__get_issue"), "{codemode}");
    // The code mode tool itself is not callable from scripts.
    assert!(!codemode.contains("### `codemode`"), "{codemode}");
    // The tool declaration carries the script grammar; pi-ai applies it only for models that support it.
    let tools = message.tools_added.clone().unwrap();
    let codemode = tools.iter().find(|tool| tool.name == "codemode").unwrap();
    let grammar = json!({ "type": "grammar", "variants": { "openai_lark": CODEMODE_SOURCE_GRAMMAR } });
    assert_eq!(codemode.constrained_sampling, Some(grammar));
    harness.close(&ctx()).await.unwrap();
}
