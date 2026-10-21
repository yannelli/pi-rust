//! Modes `on` and `only`, the inline budget, and the discovery globals scripts use to find tools.

mod support;

use std::sync::Arc;

use pi_codemode::{
    CodemodeMode, CodemodeTool, CodemodeToolInfo, CodemodeToolOptions, ToolExposure, ToolLoadout, ToolNamespace,
    describe_script_call, prepare_codemode_loadout,
};
use serde_json::{Value, json};
use support::*;

fn read_tool() -> CodemodeToolInfo {
    CodemodeToolInfo {
        prompt_guidelines: vec!["Use read to examine files instead of cat or sed.".into()],
        ..CodemodeToolInfo::new(
            "read",
            "Read a file.",
            json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
        )
    }
}

fn echo_tool() -> CodemodeToolInfo {
    CodemodeToolInfo::new("echo", "Echo text back.\n\nSecond paragraph.", json!({ "type": "object" }))
}

fn loadout() -> ToolLoadout {
    let codemode = InfoTool::shared(CodemodeToolInfo::new("codemode", "", json!({})), ToolExposure::ModelOnly);
    let tools = vec![
        InfoTool::shared(read_tool(), ToolExposure::Direct),
        InfoTool::shared(echo_tool(), ToolExposure::Direct),
        codemode,
    ];
    ToolLoadout { declared: tools.clone(), callable: tools.clone(), registered: tools, ..ToolLoadout::default() }
}

#[test]
fn presents_callable_tools_per_mode() {
    let on = prepare_codemode_loadout(&loadout(), &CodemodeToolOptions::default());
    assert_eq!(
        on.descriptions["echo"],
        "Echo text back.\n\nSecond paragraph.\n\nCodemode: `tools.echo(args)` resolves to a string."
    );
    assert!(!on.descriptions["codemode"].contains("### `echo`"));
    assert!(
        on.descriptions["codemode"].contains("`models`: classifiers and image generation. Read codemode.md first.")
    );
    assert!(on.hidden_declarations.is_empty());

    let options = CodemodeToolOptions { mode: CodemodeMode::Only, ..CodemodeToolOptions::default() };
    let only = prepare_codemode_loadout(&loadout(), &options);
    assert!(!only.descriptions.contains_key("echo"));
    let codemode = &only.descriptions["codemode"];
    assert!(codemode.contains("### `echo`") && codemode.contains("### `read`"));
    // Hidden tools' guidelines move from the system prompt to their codemode sections.
    assert!(codemode.contains("Read a file.\n\n- Use read to examine files instead of cat or sed."));
    assert_eq!(only.hidden_declarations, ["read", "echo"]);
}

#[test]
fn lists_codemode_and_deferred_exposures_by_exposure_not_activity() {
    let mut loadout = loadout();
    let mut search = CodemodeToolInfo::new("mcp__docs__search", "Search docs.", json!({}));
    search.namespace = Some(ToolNamespace::new("mcp__docs"));
    let late = CodemodeToolInfo::new("mcp__late__x", "Late.", json!({}));
    let added = [InfoTool::shared(search, ToolExposure::Codemode), InfoTool::shared(late, ToolExposure::Deferred)];
    loadout.callable.extend(added.clone());
    loadout.registered.extend(added);
    let description = &prepare_codemode_loadout(&loadout, &CodemodeToolOptions::default()).descriptions["codemode"];
    assert!(description.contains("## mcp__docs\n\n### `mcp__docs__search`"));
    assert!(!description.contains("mcp__late__x"));
    assert!(!description.contains("### `read`"));
}

#[test]
fn describes_what_script_calls_resolve_to() {
    let mut stats = CodemodeToolInfo::new("stats", "Stats.", json!({}));
    stats.output_schema = Some(json!({
        "type": "object",
        "properties": { "output": { "type": "string" }, "exit_code": { "type": "number" }, "full_output_path": { "type": "string" } },
        "required": ["output", "exit_code"],
    }));
    assert!(
        describe_script_call(&stats)
            .ends_with("`tools.stats(args)` resolves to `{ output, exit_code, full_output_path? }`.")
    );
    // The fields of object-typed fields are named one level deep, as for MCP results.
    stats.output_schema = Some(json!({
        "type": "object",
        "properties": {
            "content": { "type": "array", "items": { "type": "object" } },
            "structuredContent": {
                "type": "object",
                "properties": { "hits": { "type": "array" }, "page": { "type": "object", "properties": { "n": {} } } },
                "required": ["hits"],
            },
            "isError": { "type": "boolean" },
        },
        "required": ["content"],
    }));
    assert!(
        describe_script_call(&stats)
            .ends_with("resolves to `{ content, structuredContent?: { hits, page? }, isError? }`.")
    );
    stats.output_schema = Some(json!({
        "type": "object",
        "properties": { "content": { "type": "array" }, "structuredContent": { "type": "object" } },
        "required": ["content"],
    }));
    assert!(describe_script_call(&stats).ends_with("resolves to `{ content, structuredContent? }`."));
    stats.output_schema = Some(json!({ "type": "array", "items": { "type": "string" } }));
    assert!(describe_script_call(&stats).ends_with("resolves to `Array<string>`."));
}

#[tokio::test]
async fn shows_guidelines_of_unlisted_tools_through_describe_tool() {
    let host = Arc::new(FakeHost::default().tool(read_tool(), |_| text_outcome("hello")));
    let options = CodemodeToolOptions { inline_budget: 0, ..CodemodeToolOptions::default() };
    let tool = CodemodeTool::new(host.clone(), options.clone());
    assert!(!pi_agent::AgentTool::description(&tool).contains("### `read`"));
    let output = run_with(&host, "text(await describeTool(\"read\"))", &options).await;
    let text = result_text(&output);
    assert!(text.contains("- Use read to examine files instead of cat or sed."));
    assert!(text.contains("declare const tools: { read(args: { path: string; }): Promise<string>; };"));
}

fn mcp_host() -> FakeHost {
    let radius = ToolNamespace {
        aliases: vec!["dev-radius".into()],
        description: Some("Radius server".into()),
        instructions: Some("Use search first.".into()),
        ..ToolNamespace::new("mcp__dev-radius")
    };
    let mut search = CodemodeToolInfo::new("mcp__dev-radius__search", "Search the radius issues.", json!({}));
    search.namespace = Some(radius.clone());
    let mut fetch = CodemodeToolInfo::new("mcp__dev-radius__fetch", "Fetch one radius document.", json!({}));
    fetch.namespace = Some(radius);
    FakeHost::default()
        .tool(search, |args| text_outcome(&format!("found {}", args["q"].as_str().unwrap_or_default())))
        .tool(fetch, |_| text_outcome("doc"))
        .tool(CodemodeToolInfo::new("list_issues", "List issues in the tracker.", json!({})), |_| text_outcome("[]"))
}

#[tokio::test]
async fn finds_tools_with_search_tools_and_describe_namespace() {
    let host = Arc::new(mcp_host());
    let code = r#"
        const found = await searchTools("issues");
        const scoped = await searchTools("issues", { namespace: "dev-radius", limit: 5 });
        const errors = [];
        for (const run of [() => searchTools(1), () => searchTools("x", { limit: 0 }), () => searchTools("x", { namespace: 3 })]) {
            try { await run(); } catch (error) { errors.push(error.message); }
        }
        return {
            found: found.map((tool) => tool.name),
            scoped: scoped.map((tool) => tool.name),
            sample: found[0].description.includes("codemode tool declaration:"),
            namespace: await describeNamespace("dev_radius"),
            missing: (await describeNamespace("nope")) === undefined,
            unknownTool: (await describeTool("nope")) === undefined,
            byId: (await describeTool("mcp__dev_radius__fetch")).startsWith("Fetch one radius document."),
            called: await tools.mcp__dev_radius__search({ q: "x" }),
            errors,
        };
    "#;
    let value: Value = serde_json::from_str(&result_text(&run_script(&host, code).await)).unwrap();
    assert_eq!(value["found"], json!(["list_issues", "mcp__dev_radius__search"]));
    assert_eq!(value["scoped"], json!(["mcp__dev_radius__search"]));
    assert_eq!(value["sample"], json!(true));
    assert_eq!(
        value["namespace"],
        json!({
            "name": "mcp__dev-radius",
            "description": "Radius server",
            "instructions": "Use search first.",
            "tools": ["mcp__dev_radius__search", "mcp__dev_radius__fetch"],
        })
    );
    assert_eq!(
        (value["missing"].clone(), value["unknownTool"].clone(), value["byId"].clone()),
        (json!(true), json!(true), json!(true))
    );
    assert_eq!(value["called"], json!("found x"));
    assert_eq!(
        value["errors"],
        json!([
            "searchTools() expects a query string",
            "searchTools() limit must be a positive integer",
            "searchTools() namespace must be a string",
        ])
    );
}
