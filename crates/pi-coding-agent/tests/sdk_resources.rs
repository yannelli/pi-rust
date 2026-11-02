//! SDK `DefaultResourceLoader` against upstream `resource-loader.test.ts`, `resource-loader-theme.test.ts`
//! and the #2753 regression (upstream 35dfa757), and against the native loader on the same tree.

#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "agent_session/mcp_fake.rs"]
#[allow(dead_code)]
mod mcp_fake;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

#[path = "sdk_resources/limits.rs"]
mod limits;

use std::path::{Path, PathBuf};

use harness::{Harness, Setup, harness};
use pi_coding_agent::agent_session::services::{ResourceOptions, resource_loader_options};
use pi_coding_agent::resources::ResourceLoader;
use pi_coding_agent::settings::{SettingsManager, SettingsOptions};
use serde_json::{Value, json};

const PRELUDE: &str = r#"
import { DefaultResourceLoader, SettingsManager } from '@earendil-works/pi-coding-agent';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
const fail = async (run) => { try { await run(); return null; } catch (error) { return error.message; } };
const pick = ({ name, sourcePath, sourceInfo }) => ({ name, sourcePath, sourceInfo });
export default function (pi) {
  pi.registerCommand('probe', { handler: async (_args, ctx) => {
    const root = join(ctx.cwd, '..');
    process.env.HOME = join(root, 'home');
    process.env.USERPROFILE = process.env.HOME;
    const agentDir = join(root, 'agent');
    const missing = { skill: join(root, 'missing-skill'), prompt: join(root, 'missing.md'), theme: join(root, 'missing-theme.json') };
    const result = {};
"#;
const EPILOGUE: &str = "\n    pi.appendEntry('result', result);\n  }});\n}\n";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn skill(dir: &Path, name: &str) {
    write(&dir.join("SKILL.md"), &format!("---\nname: {name}\ndescription: {name} skill\n---\nContent"));
}

fn theme_json(name: &str) -> String {
    let mut theme: Value = serde_json::from_str(include_str!("../src/theme/builtin/dark.json")).unwrap();
    theme["name"] = json!(name);
    theme["colors"]["userMessageBg"] = json!("#3c3544");
    theme.to_string()
}

async fn run(body: &str, before: impl FnOnce(&Harness, &Path)) -> (Harness, PathBuf, Value) {
    let source = [PRELUDE, body, EPILOGUE].concat();
    let h = harness(Setup { files: vec![("probe.ts", source)], load: vec!["probe.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let root = h.cwd.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(root.join("home")).unwrap();
    before(&h, &root);
    tokio::time::timeout(std::time::Duration::from_secs(20), h.session.prompt("/probe", Vec::new()))
        .await
        .unwrap()
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entries = h.entries();
    let data = entries.iter().find(|entry| entry["customType"] == "result").expect("result entry")["data"].clone();
    (h, root, data)
}

fn names(list: &Value) -> Vec<&str> {
    list.as_array().unwrap().iter().map(|item| item["name"].as_str().unwrap()).collect()
}

fn has(diagnostics: &Value, kind: &str, message: &str, path: &Path) -> bool {
    let wanted = json!({"type": kind, "message": message, "path": path.display().to_string()});
    diagnostics.as_array().unwrap().contains(&wanted)
}

const LIFECYCLE: &str = r#"
    const order = [];
    const track = (kind) => (base) => { order.push(kind); return base; };
    const loader = new DefaultResourceLoader({ cwd: ctx.cwd, agentDir, additionalSkillPaths: [missing.skill],
      additionalPromptTemplatePaths: [missing.prompt], additionalThemePaths: [missing.theme],
      skillsOverride: track('skills'), promptsOverride: track('prompts'), themesOverride: track('themes'),
      agentsFilesOverride: track('agentsFiles'), systemPromptOverride: track('systemPrompt'),
      appendSystemPromptOverride: track('appendSystemPrompt') });
    result.before = { skills: loader.getSkills(), prompts: loader.getPrompts(), themes: loader.getThemes(),
      agentsFiles: loader.getAgentsFiles(), extensions: loader.getExtensions().extensions,
      systemPrompt: loader.getSystemPrompt() ?? null };
    await loader.reload();
    const skills = loader.getSkills();
    result.stable = skills.skills === loader.getSkills().skills && skills.diagnostics === loader.getSkills().diagnostics;
    result.reloaded = { skills, prompts: loader.getPrompts(), themes: loader.getThemes().themes.map(pick),
      themeDiagnostics: loader.getThemes().diagnostics, agentsFiles: loader.getAgentsFiles().agentsFiles,
      systemPrompt: loader.getSystemPrompt(), source: loader.getSystemPromptSource(),
      warnings: loader.getExtensions().warnings };
    result.order = [...order];
    const meta = (source, baseDir) => ({ source, scope: 'temporary', origin: 'top-level', baseDir });
    const extra = join(root, 'extra-skills', 'extra-skill');
    const fileUrl = join(root, 'extra skills', 'file-url-skill');
    loader.extendResources({
      skillPaths: [{ path: extra, metadata: meta('extension:extra', extra) },
        { path: 'file://' + encodeURI(fileUrl), metadata: meta('extension:file-url', fileUrl) }],
      promptPaths: [{ path: join(root, 'extra-prompts', 'extra.md'), metadata: meta('extension:extra', join(root, 'extra-prompts')) }],
    });
    result.extendOrder = order.slice(result.order.length);
    result.extended = { skills: loader.getSkills(), prompts: loader.getPrompts() };
    result.options = await fail(() => loader._sdkOptions());
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_matches_upstream_order_and_the_native_loader() {
    let (h, root, data) = run(LIFECYCLE, |h, root| {
        skill(&h.agent_dir.join("skills/base-skill"), "base-skill");
        write(&h.agent_dir.join("prompts/base.md"), "---\ndescription: Base prompt\n---\nBase");
        write(&h.agent_dir.join("themes/mine.json"), &theme_json("mine"));
        write(&h.agent_dir.join("SYSTEM.md"), "Global system prompt.");
        write(&h.cwd.join("AGENTS.md"), "Project instructions");
        skill(&root.join("extra-skills/extra-skill"), "extra-skill");
        skill(&root.join("extra skills/file-url-skill"), "file-url-skill");
        write(&root.join("extra-prompts/extra.md"), "---\ndescription: Extra prompt\n---\nExtra prompt content");
    })
    .await;
    let empty = |key: &str| {
        let mut value = json!({"diagnostics": []});
        value[key] = json!([]);
        value
    };
    assert_eq!(data["before"]["skills"], empty("skills"));
    assert_eq!(data["before"]["prompts"], empty("prompts"));
    assert_eq!(data["before"]["themes"], empty("themes"));
    assert_eq!(data["before"]["agentsFiles"], json!({"agentsFiles": []}));
    assert_eq!(data["before"]["extensions"], json!([]));
    assert_eq!(data["before"]["systemPrompt"], Value::Null);
    assert_eq!(data["stable"], true);
    let order = ["skills", "prompts", "themes", "agentsFiles", "systemPrompt", "appendSystemPrompt"];
    assert_eq!(data["order"], json!(order));
    assert_eq!(data["extendOrder"], json!(["skills", "prompts"]));

    let reloaded = &data["reloaded"];
    let missing = |name: &str| root.join(name);
    let settings = SettingsManager::create(&h.cwd, &h.agent_dir, SettingsOptions { project_trusted: true });
    let path = |name: &str| missing(name).display().to_string();
    let resources = ResourceOptions {
        skill_paths: vec![path("missing-skill")],
        prompt_template_paths: vec![path("missing.md")],
        theme_paths: vec![path("missing-theme.json")],
        home: Some(root.join("home")),
        ..ResourceOptions::default()
    };
    let mut options = resource_loader_options(&h.cwd, &h.agent_dir, &settings, &resources);
    options.builtin_extensions.clear();
    let mut native = ResourceLoader::new(options);
    native.reload();
    let loaded = native.resources();
    assert_eq!(reloaded["skills"], json!({"skills": loaded.skills, "diagnostics": loaded.skill_diagnostics}));
    assert_eq!(reloaded["prompts"], json!({"prompts": loaded.prompts, "diagnostics": loaded.prompt_diagnostics}));
    let themes: Vec<Value> = loaded
        .themes
        .iter()
        .map(|theme| json!({"name": theme.name, "sourcePath": theme.path, "sourceInfo": theme.source_info}))
        .collect();
    assert_eq!(reloaded["themes"], json!(themes));
    assert_eq!(reloaded["themeDiagnostics"], json!(loaded.theme_diagnostics));
    let files: Vec<Value> =
        loaded.context_files.iter().map(|file| json!({"path": file.path, "content": file.content})).collect();
    assert_eq!(reloaded["agentsFiles"], json!(files));
    assert_eq!(names(&reloaded["skills"]["skills"]), ["base-skill"]);
    assert_eq!(names(&reloaded["themes"]), ["mine"]);
    assert_eq!(reloaded["systemPrompt"], "Global system prompt.");
    assert_eq!(reloaded["source"], json!({"path": h.agent_dir.join("SYSTEM.md")}));
    assert_eq!(reloaded["warnings"], json!([]));

    let prompt_diagnostics = &reloaded["prompts"]["diagnostics"];
    assert!(has(prompt_diagnostics, "error", "Prompt template path does not exist", &missing("missing.md")));
    let skill_diagnostics = &reloaded["skills"]["diagnostics"];
    assert!(has(skill_diagnostics, "warning", "skill path does not exist", &missing("missing-skill")));
    assert!(!has(skill_diagnostics, "error", "Skill path does not exist", &missing("missing-skill")));
    let theme_diagnostics = &reloaded["themeDiagnostics"];
    assert!(has(theme_diagnostics, "warning", "theme path does not exist", &missing("missing-theme.json")));
    assert!(!has(theme_diagnostics, "error", "Theme path does not exist", &missing("missing-theme.json")));

    let extended = &data["extended"];
    let find = |list: &Value, name: &str| list.as_array().unwrap().iter().find(|item| item["name"] == name).cloned();
    let extra = find(&extended["skills"]["skills"], "extra-skill").expect("extra skill");
    assert_eq!(extra["sourceInfo"]["source"], "extension:extra");
    assert_eq!(extra["sourceInfo"]["path"], json!(root.join("extra-skills").join("extra-skill").join("SKILL.md")));
    let file_url = find(&extended["skills"]["skills"], "file-url-skill").expect("file URL skill");
    assert_eq!(file_url["filePath"], json!(root.join("extra skills").join("file-url-skill").join("SKILL.md")));
    assert_eq!(file_url["sourceInfo"]["source"], "extension:file-url");
    let prompt = find(&extended["prompts"]["prompts"], "extra").expect("extra prompt");
    assert_eq!(prompt["sourceInfo"]["source"], "extension:extra");
    assert_eq!(prompt["sourceInfo"]["path"], json!(root.join("extra-prompts").join("extra.md")));
    let rerun = &extended["prompts"]["diagnostics"];
    assert!(!has(rerun, "error", "Prompt template path does not exist", &missing("missing.md")));
    assert_eq!(data["options"], "createAgentSession.resourceLoader.skillsOverride is not supported in pi-rust");
}

const OVERRIDES: &str = r#"
    const given = { path: '/fake/path', source: 'custom', scope: 'temporary', origin: 'top-level' };
    const injected = { name: 'injected', description: 'Injected skill', filePath: '/fake/path', baseDir: '/fake',
      sourceInfo: given, disableModelInvocation: false, extra: 'kept' };
    const diagnostics = [];
    const custom = Object.assign(Object.create({ tag() { return 'proto'; } }),
      { name: 'custom', sourcePath: join(agentDir, 'themes', 'mine.json') });
    const bare = { name: 'bare', sourceInfo: given };
    const themes = [custom, bare];
    const files = [{ path: '/virtual/AGENTS.md', content: 'Virtual' }];
    const loader = new DefaultResourceLoader({ cwd: ctx.cwd, agentDir, additionalSkillPaths: [missing.skill],
      skillsOverride: () => ({ skills: [injected], diagnostics }),
      themesOverride: (base) => ({ themes, diagnostics: base.diagnostics }),
      agentsFilesOverride: () => ({ agentsFiles: files }), systemPromptOverride: () => 'Custom system prompt' });
    await loader.reload();
    const stored = loader.getSkills().skills[0];
    const loaded = loader.getThemes().themes;
    result.skills = { count: loader.getSkills().skills.length, name: stored.name, copied: stored !== injected,
      extra: stored.extra, sameInfo: stored.sourceInfo === given, sameDiagnostics: loader.getSkills().diagnostics === diagnostics,
      diagnostics };
    result.themes = { newArray: loaded !== themes, same: loaded[0] === custom && loaded[1] === bare,
      tag: loaded[0].tag(), info: custom.sourceInfo, bareInfo: bare.sourceInfo === given };
    result.agentsFiles = loader.getAgentsFiles().agentsFiles === files;
    result.systemPrompt = loader.getSystemPrompt();
    const noSkills = new DefaultResourceLoader({ cwd: ctx.cwd, agentDir, noSkills: true,
      additionalSkillPaths: [join(root, 'custom-skills')] });
    await noSkills.reload();
    result.noSkills = noSkills.getSkills().skills.map((skill) => skill.name);
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overrides_keep_their_objects_and_get_missing_errors() {
    let (h, root, data) = run(OVERRIDES, |h, root| {
        write(&h.agent_dir.join("themes/mine.json"), &theme_json("mine"));
        write(&root.join("custom-skills/custom.md"), "---\nname: custom\ndescription: Custom skill\n---\nContent");
    })
    .await;
    let skills = &data["skills"];
    assert_eq!((skills["count"].clone(), skills["name"].clone()), (json!(1), json!("injected")));
    assert_eq!([&skills["copied"], &skills["sameInfo"], &skills["sameDiagnostics"]], [&json!(true); 3]);
    assert_eq!(skills["extra"], "kept");
    let missing = root.join("missing-skill").display().to_string();
    let error = json!({"type": "error", "message": "Skill path does not exist", "path": missing});
    assert_eq!(skills["diagnostics"], json!([error]));
    let themes = &data["themes"];
    assert_eq!([&themes["newArray"], &themes["same"], &themes["bareInfo"]], [&json!(true); 3]);
    assert_eq!(themes["tag"], "proto");
    assert_eq!(themes["info"]["path"], json!(h.agent_dir.join("themes").join("mine.json")));
    assert_eq!(data["agentsFiles"], true);
    assert_eq!(data["systemPrompt"], "Custom system prompt");
    assert_eq!(data["noSkills"], json!(["custom"]));
}

const TRUST: &str = r#"
    const untrusted = SettingsManager.create(ctx.cwd, agentDir, { projectTrusted: false });
    const loader = new DefaultResourceLoader({ cwd: ctx.cwd, agentDir, settingsManager: untrusted });
    const view = () => ({ systemPrompt: loader.getSystemPrompt(), agents: loader.getAgentsFiles().agentsFiles.map((file) => file.path),
      skills: loader.getSkills().skills.map((skill) => skill.name), prompts: loader.getPrompts().prompts.map((prompt) => prompt.name) });
    await loader.reload();
    result.untrusted = view();
    untrusted.setProjectTrusted(true);
    await loader.reload();
    result.trusted = view();
    writeFileSync(join(agentDir, 'settings.json'), JSON.stringify({ prompts: ['-prompts/test.md'] }));
    await loader.reload();
    result.stale = view();
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trust_and_changed_settings_apply_on_reload() {
    let (h, _root, data) = run(TRUST, |h, _| {
        write(&h.cwd.join(".pi/SYSTEM.md"), "Project system prompt.");
        write(&h.agent_dir.join("SYSTEM.md"), "Global system prompt.");
        write(&h.agent_dir.join("AGENTS.md"), "Global instructions");
        write(&h.cwd.join("AGENTS.md"), "Project instructions");
        skill(&h.cwd.join(".pi/skills/project-skill"), "project-skill");
        write(&h.agent_dir.join("prompts/test.md"), "Echo test prompt\n");
    })
    .await;
    let agents = json!([h.agent_dir.join("AGENTS.md"), h.cwd.join("AGENTS.md")]);
    let untrusted = &data["untrusted"];
    assert_eq!(untrusted["systemPrompt"], "Global system prompt.");
    assert!(agents.as_array().unwrap().iter().all(|path| untrusted["agents"].as_array().unwrap().contains(path)));
    assert!(!untrusted["skills"].as_array().unwrap().contains(&json!("project-skill")));
    assert!(untrusted["prompts"].as_array().unwrap().contains(&json!("test")));
    assert_eq!(data["trusted"]["systemPrompt"], "Project system prompt.");
    assert!(data["trusted"]["skills"].as_array().unwrap().contains(&json!("project-skill")));
    assert!(!data["stale"]["prompts"].as_array().unwrap().contains(&json!("test")));
}
