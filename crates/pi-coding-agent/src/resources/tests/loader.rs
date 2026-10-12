//! Port of upstream `test/resource-loader.test.ts` (everything except extension loading).

use std::path::{Path, PathBuf};

use super::{skill_md, write};
use crate::resources::loader::ExtensionResourcePaths;
use crate::resources::source_info::{PathMetadata, SourceOrigin};
use crate::resources::{
    DiagnosticKind, ResolvedPaths, ResolvedResource, ResourceLoader, ResourceLoaderOptions, ResourceSettings,
    SourceScope,
};

struct Dirs {
    temp: tempfile::TempDir,
    agent: PathBuf,
    cwd: PathBuf,
}

fn dirs() -> Dirs {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let cwd = temp.path().join("project");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    Dirs { agent, cwd, temp }
}

fn options(dirs: &Dirs) -> ResourceLoaderOptions {
    let mut options = ResourceLoaderOptions::new(&dirs.cwd, &dirs.agent);
    options.home = dirs.temp.path().join("home");
    options
}

fn load(options: ResourceLoaderOptions) -> ResourceLoader {
    let mut loader = ResourceLoader::new(options);
    loader.reload();
    loader
}

#[test]
fn starts_empty_and_discovers_user_skills_and_prompts() {
    let d = dirs();
    let loader = ResourceLoader::new(options(&d));
    assert!(loader.resources().skills.is_empty() && loader.resources().prompts.is_empty());

    write(&d.agent.join("skills/test-skill.md"), &skill_md("test-skill", "A test skill"));
    let browser = d.agent.join("skills/pi-skills/browser-tools");
    write(&browser.join("SKILL.md"), &skill_md("browser-tools", "Browser tools"));
    write(&browser.join("EFFICIENCY.md"), "No frontmatter here");
    write(&d.agent.join("prompts/test-prompt.md"), "---\ndescription: A test prompt\n---\nPrompt content.");
    let loader = load(options(&d));
    let resources = loader.resources();
    let names: Vec<&str> = resources.skills.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(names, ["browser-tools", "test-skill"]);
    assert!(!resources.skill_diagnostics.iter().any(|d| d.path.as_deref().unwrap_or("").ends_with("EFFICIENCY.md")));
    assert_eq!(resources.skills[0].source_info.source, "auto");
    assert_eq!(resources.skills[0].source_info.scope, SourceScope::User);
    assert_eq!(resources.prompts[0].name, "test-prompt");
    assert_eq!(resources.prompts[0].description, "A test prompt");
}

#[test]
fn reports_invalid_prompt_frontmatter_and_keeps_siblings() {
    let d = dirs();
    let invalid = d.agent.join("prompts/invalid.md");
    write(&invalid, "---\ndescription: Broken: unquoted colon\n---\nDo something.\n");
    write(&d.agent.join("prompts/valid.md"), "Valid prompt content.");
    let loader = load(options(&d));
    let resources = loader.resources();
    assert_eq!(resources.prompts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["valid"]);
    assert_eq!(resources.prompt_diagnostics.len(), 1);
    assert_eq!(resources.prompt_diagnostics[0].kind, DiagnosticKind::Warning);
    assert!(resources.prompt_diagnostics[0].path_is(&invalid));
    assert!(resources.prompt_diagnostics[0].message.contains("line 1, column "));
}

#[test]
fn project_resources_win_name_collisions() {
    let d = dirs();
    let (user_prompt, project_prompt) = (d.agent.join("prompts/commit.md"), d.cwd.join(".pi/prompts/commit.md"));
    write(&user_prompt, "User prompt");
    write(&project_prompt, "Project prompt");
    let project_skill = d.cwd.join(".pi/skills/collision-skill/SKILL.md");
    write(&d.agent.join("skills/collision-skill/SKILL.md"), &skill_md("collision-skill", "user"));
    write(&project_skill, &skill_md("collision-skill", "project"));
    let project_theme = d.cwd.join(".pi/themes/collision.json");
    write(&d.agent.join("themes/collision.json"), r#"{"name": "collision-theme"}"#);
    write(&project_theme, r##"{"name": "collision-theme", "vars": {"accent": "#ff00ff"}}"##);
    let loader = load(options(&d));
    let resources = loader.resources();
    assert_eq!(resources.prompts[0].file_path, project_prompt);
    assert_eq!(resources.skills[0].file_path, project_skill);
    assert_eq!(resources.themes[0].path, project_theme);
    assert_eq!(resources.themes[0].name.as_deref(), Some("collision-theme"));
    for diagnostics in [&resources.prompt_diagnostics, &resources.skill_diagnostics, &resources.theme_diagnostics] {
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].kind, DiagnosticKind::Collision);
    }
    assert_eq!(resources.prompt_diagnostics[0].message, "name \"/commit\" collision");
}

#[test]
fn honors_settings_overrides_for_auto_discovered_resources() {
    let d = dirs();
    write(&d.agent.join("extensions/disabled.ts"), "export default function() {}");
    write(&d.agent.join("skills/skip-skill/SKILL.md"), &skill_md("skip-skill", "Skip me"));
    write(&d.agent.join("prompts/skip.md"), "Skip prompt");
    write(&d.agent.join("themes/skip.json"), "{}");
    let mut options = options(&d);
    options.user_settings = ResourceSettings {
        extensions: vec!["-extensions/disabled.ts".into()],
        skills: vec!["-skills/skip-skill".into()],
        prompts: vec!["-prompts/skip.md".into()],
        themes: vec!["-themes/skip.json".into()],
    };
    let loader = load(options);
    let resources = loader.resources();
    assert!(resources.extensions.is_empty() && resources.skills.is_empty());
    assert!(resources.prompts.is_empty() && resources.themes.is_empty());
}

#[test]
fn loads_context_files_unless_disabled() {
    let d = dirs();
    write(&d.cwd.join("AGENTS.md"), "# Project Guidelines\n\nBe helpful.");
    let loader = load(options(&d));
    assert!(loader.resources().context_files.iter().any(|file| file.path == d.cwd.join("AGENTS.md")));
    let loader = load(ResourceLoaderOptions { no_context_files: true, ..options(&d) });
    assert!(loader.resources().context_files.is_empty());
}

#[test]
fn discovers_system_prompt_files_and_sources() {
    let d = dirs();
    let project_system = d.cwd.join(".pi/SYSTEM.md");
    let project_append = d.cwd.join(".pi/APPEND_SYSTEM.md");
    write(&d.agent.join("SYSTEM.md"), "Global system prompt.");
    let loader = load(options(&d));
    assert_eq!(loader.resources().system_prompt.as_deref(), Some("Global system prompt."));
    assert_eq!(loader.resources().system_prompt_source.as_deref(), Some(d.agent.join("SYSTEM.md").as_path()));

    write(&project_system, "\u{feff}Project system prompt.");
    write(&project_append, "Project append prompt.");
    let loader = load(options(&d));
    let resources = loader.resources();
    assert_eq!(resources.system_prompt.as_deref(), Some("Project system prompt."));
    assert_eq!(resources.system_prompt_source.as_deref(), Some(project_system.as_path()));
    assert_eq!(resources.append_system_prompt, ["Project append prompt."]);
    assert_eq!(resources.append_system_prompt_sources, std::slice::from_ref(&project_append));

    let literal = load(ResourceLoaderOptions {
        system_prompt: Some("Literal system prompt.".into()),
        append_system_prompt: Some(vec!["Literal append prompt.".into()]),
        ..options(&d)
    });
    assert_eq!(literal.resources().system_prompt.as_deref(), Some("Literal system prompt."));
    assert_eq!(literal.resources().system_prompt_source, None);
    assert_eq!(literal.resources().append_system_prompt_sources, Vec::<PathBuf>::new());

    let custom = d.temp.path().join("custom-append.md");
    write(&custom, "Custom append prompt.");
    let files = load(ResourceLoaderOptions {
        system_prompt: Some(project_append.display().to_string()),
        append_system_prompt: Some(vec![custom.display().to_string(), "Literal append prompt.".into()]),
        ..options(&d)
    });
    assert_eq!(files.resources().system_prompt.as_deref(), Some("Project append prompt."));
    assert_eq!(files.resources().append_system_prompt, ["Custom append prompt.", "Literal append prompt."]);
    assert_eq!(files.resources().append_system_prompt_text(), "Custom append prompt.\n\nLiteral append prompt.");
    assert_eq!(files.resources().append_system_prompt_sources, [custom]);
}

#[test]
fn untrusted_projects_skip_protected_resources() {
    let d = dirs();
    let pi = d.cwd.join(".pi");
    write(&pi.join("SYSTEM.md"), "Project system prompt.");
    write(&pi.join("APPEND_SYSTEM.md"), "Project append.");
    write(&d.agent.join("SYSTEM.md"), "Global system prompt.");
    write(&d.agent.join("AGENTS.md"), "Global instructions");
    write(&d.cwd.join("AGENTS.md"), "Project instructions");
    write(&pi.join("extensions/project.ts"), "throw new Error(\"should not load\");");
    write(&pi.join("skills/project-skill/SKILL.md"), &skill_md("project-skill", "Project skill"));
    write(&pi.join("prompts/project.md"), "Project prompt");
    write(&pi.join("themes/project.json"), r#"{"name": "project-theme"}"#);
    write(&d.cwd.join(".agents/skills/agents-skill/SKILL.md"), &skill_md("agents-skill", "Agents"));
    let mut options = options(&d);
    options.project_trusted = false;
    options.project_settings.prompts = vec!["prompts".into()];
    let loader = load(options);
    let resources = loader.resources();
    assert_eq!(resources.system_prompt.as_deref(), Some("Global system prompt."));
    assert!(resources.append_system_prompt.is_empty());
    assert_eq!(resources.context_files.len(), 2, "context files load without trust");
    assert!(resources.extensions.is_empty() && resources.extension_diagnostics.is_empty());
    assert!(resources.skills.is_empty() && resources.prompts.is_empty() && resources.themes.is_empty());
}

#[test]
fn no_skills_still_loads_explicit_paths() {
    let d = dirs();
    write(&d.agent.join("skills/auto/SKILL.md"), &skill_md("auto", "Auto"));
    let explicit = d.temp.path().join("explicit/SKILL.md");
    write(&explicit, &skill_md("explicit", "Explicit"));
    let loader = load(ResourceLoaderOptions { no_skills: true, ..options(&d) });
    assert!(loader.resources().skills.is_empty());
    let mut options = ResourceLoaderOptions { no_skills: true, no_prompt_templates: true, ..options(&d) };
    options.skill_paths = vec![explicit.parent().unwrap().display().to_string(), "missing-skill".into()];
    options.prompt_template_paths = vec!["missing.md".into()];
    options.theme_paths = vec!["missing.json".into()];
    let loader = load(options);
    let resources = loader.resources();
    assert_eq!(resources.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["explicit"]);
    assert_eq!(resources.skills[0].source_info.scope, SourceScope::Temporary);
    assert!(resources.skill_diagnostics.iter().any(|d| d.message == "skill path does not exist"));
    assert!(resources.prompt_diagnostics.iter().any(|d| d.message == "Prompt template path does not exist"));
    assert!(resources.theme_diagnostics.iter().any(|d| d.message == "theme path does not exist"));
}

fn package_resource(path: &Path, root: &Path) -> ResolvedResource {
    let mut metadata = PathMetadata::new("npm:pkg", SourceScope::User, SourceOrigin::Package).with_base_dir(root);
    metadata.package_root = Some(root.to_path_buf());
    ResolvedResource { path: path.to_path_buf(), enabled: true, metadata }
}

#[test]
fn keeps_package_and_extension_metadata() {
    let d = dirs();
    let root = d.temp.path().join("pkg");
    let skill_dir = root.join("skills/pkg-skill");
    write(&skill_dir.join("SKILL.md"), &skill_md("pkg-skill", "Package skill"));
    write(&root.join("prompts/pkg.md"), "Package prompt");
    write(&root.join("themes/pkg.json"), r#"{"name": "pkg-theme"}"#);
    let mut options = options(&d);
    options.package_resources = ResolvedPaths {
        skills: vec![package_resource(&skill_dir, &root)],
        prompts: vec![package_resource(&root.join("prompts/pkg.md"), &root)],
        themes: vec![package_resource(&root.join("themes/pkg.json"), &root)],
        ..Default::default()
    };
    let mut loader = load(options);
    let resources = loader.resources();
    assert_eq!(resources.skills[0].name, "pkg-skill");
    for info in [&resources.skills[0].source_info, &resources.prompts[0].source_info, &resources.themes[0].source_info]
    {
        assert_eq!(info.source, "npm:pkg");
        assert_eq!(info.origin, SourceOrigin::Package);
    }

    let ext_dir = d.temp.path().join("ext-resources");
    write(&ext_dir.join("skills/ext-skill/SKILL.md"), &skill_md("ext-skill", "Extension skill"));
    write(&ext_dir.join("prompts/ext.md"), "Extension prompt");
    let metadata = PathMetadata::new("extension:demo", SourceScope::Temporary, SourceOrigin::TopLevel);
    loader.extend_resources(ExtensionResourcePaths {
        skill_paths: vec![(ext_dir.join("skills"), metadata.clone())],
        prompt_paths: vec![(ext_dir.join("prompts"), metadata)],
        theme_paths: Vec::new(),
    });
    let resources = loader.resources();
    let skill = resources.skills.iter().find(|skill| skill.name == "ext-skill").unwrap();
    assert_eq!(skill.source_info.source, "extension:demo");
    let prompt = resources.prompts.iter().find(|prompt| prompt.name == "ext").unwrap();
    assert_eq!(prompt.source_info.source, "extension:demo");
    assert_eq!(resources.prompts.len(), 2);
}

#[test]
fn collects_extension_paths_with_source_info() {
    let d = dirs();
    write(&d.agent.join("extensions/user.ts"), "");
    write(&d.cwd.join(".pi/extensions/project/index.js"), "");
    let cli_file = d.temp.path().join("cli.ts");
    write(&cli_file, "");
    let mut options = options(&d);
    options.extension_sources = vec![cli_file.display().to_string(), "./missing.ts".into(), "builtin:mcp".into()];
    options.builtin_extensions = vec!["mcp".into(), "other".into()];
    options.disabled_builtin_extensions = vec!["other".into()];
    let loader = load(options.clone());
    let resources = loader.resources();
    let paths: Vec<String> = resources.extensions.iter().map(|e| e.path.display().to_string()).collect();
    let expected = [
        cli_file.display().to_string(),
        "builtin:mcp".into(),
        d.cwd.join(".pi/extensions/project/index.js").display().to_string(),
        d.agent.join("extensions/user.ts").display().to_string(),
    ];
    assert_eq!(paths, expected);
    assert_eq!(resources.extensions[0].source_info.source, "cli");
    assert_eq!(resources.extensions[1].source_info.source, "builtin");
    assert_eq!(resources.extensions[2].source_info.scope, SourceScope::Project);
    assert_eq!(resources.extensions[3].source_info.source, "auto");
    let missing = d.cwd.join("missing.ts");
    assert_eq!(
        resources.extension_diagnostics[0].message,
        format!("Extension path does not exist: {}", missing.display())
    );

    let loader = load(ResourceLoaderOptions { no_extensions: true, ..options });
    assert_eq!(loader.resources().extensions.len(), 2, "only -e extensions load with --no-extensions");
}
