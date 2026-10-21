//! Discovery and filter tests, following upstream `package-manager.ts` behavior.

use std::path::{Path, PathBuf};

use super::{skill_md, write};
use crate::resources::discovery::{
    LocalResourceOptions, ResourceSettings, SkillDiscoveryMode, ancestor_agents_skill_dirs, collect_skill_entries,
    resolve_local_resources,
};
use crate::resources::filters::{apply_patterns, is_enabled_by_overrides, is_pattern};
use crate::resources::{ResolvedPaths, ResolvedResource, SourceScope};

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn enabled_names(list: &[ResolvedResource]) -> Vec<String> {
    list.iter()
        .filter(|resource| resource.enabled)
        .map(|resource| resource.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn patterns_include_exclude_and_force() {
    let base = Path::new("/base");
    let files: Vec<PathBuf> =
        ["/base/a.md", "/base/b.md", "/base/sub/c.md", "/base/skills/x/SKILL.md"].iter().map(PathBuf::from).collect();
    let enabled = |patterns: &[&str]| {
        let set = apply_patterns(&files, &strings(patterns), base);
        let mut list: Vec<String> = set.iter().map(|path| path.display().to_string()).collect();
        list.sort();
        list
    };
    assert_eq!(enabled(&[]).len(), 4);
    assert_eq!(enabled(&["*.md"]).len(), 4, "file names match too");
    assert_eq!(enabled(&["sub/*.md"]), ["/base/sub/c.md"]);
    assert_eq!(enabled(&["**/*.md", "!b.md"]).len(), 3);
    assert_eq!(enabled(&["!*.md", "+b.md"]), ["/base/b.md"]);
    assert_eq!(enabled(&["+./a.md", "-a.md"]).len(), 3);
    assert!(!enabled(&["!x"]).contains(&"/base/skills/x/SKILL.md".to_string()), "skill directories match by name");
    assert!(!enabled(&["-skills/x"]).contains(&"/base/skills/x/SKILL.md".to_string()));

    let skill = Path::new("/base/skills/x/SKILL.md");
    assert!(!is_enabled_by_overrides(skill, &strings(&["!x"]), base));
    assert!(is_enabled_by_overrides(skill, &strings(&["!x", "+skills/x"]), base));
    assert!(!is_enabled_by_overrides(skill, &strings(&["+skills/x", "-/base/skills/x"]), base));
    assert!(is_pattern("!a") && is_pattern("a*") && is_pattern("-a") && !is_pattern("dir/file.md"));
}

// Upstream #10765: `!~/skills/x`, `+~/x`, and `-~/x` were compared with a literal `~`.
#[test]
fn patterns_expand_a_leading_tilde() {
    let home = crate::resources::paths::home_dir();
    let base = home.join("project");
    let skills = home.join("extra/skills");
    let files: Vec<PathBuf> = ["keep", "glob-excluded", "exact-excluded", "exact-included"]
        .iter()
        .map(|name| skills.join(name).join("SKILL.md"))
        .collect();
    let names = |patterns: &[&str]| {
        let mut names: Vec<String> = apply_patterns(&files, &strings(patterns), &base)
            .iter()
            .map(|path| path.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    assert_eq!(names(&["!~/extra/skills/glob-*"]), ["exact-excluded", "exact-included", "keep"]);
    assert_eq!(names(&["-~/extra/skills/exact-excluded"]), ["exact-included", "glob-excluded", "keep"]);
    assert_eq!(names(&["~/extra/skills/keep"]).len(), 1, "plain include patterns expand too");
    assert_eq!(names(&["!~/extra/skills/*", "+~/extra/skills/exact-included"]), ["exact-included"]);

    let skill = skills.join("keep/SKILL.md");
    assert!(!is_enabled_by_overrides(&skill, &strings(&["!~/extra/skills/keep"]), &base));
    assert!(is_enabled_by_overrides(&skill, &strings(&["!~/extra/skills/*", "+~/extra/skills/keep"]), &base));
    assert!(!is_enabled_by_overrides(&skill, &strings(&["-~/extra/skills/keep"]), &base));
}

#[test]
fn pattern_text_expands_home_and_file_urls_only() {
    use crate::resources::paths::normalize_path_input;
    let home = crate::resources::paths::home_dir();
    assert_eq!(normalize_path_input("~"), home.to_string_lossy());
    assert_eq!(normalize_path_input("~/a/../b/*.md"), home.join("b").join("*.md").to_string_lossy());
    for unchanged in ["~user/x", "skills/*", "**/*.md", "x~/y", "/abs/~/z"] {
        assert_eq!(normalize_path_input(unchanged), unchanged);
    }
    #[cfg(unix)]
    assert_eq!(normalize_path_input("file:///opt/my%20skills"), "/opt/my skills");
}

#[test]
fn agents_skill_dirs_stop_at_the_repo_root() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let nested = repo.join("a/b");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    let dirs = ancestor_agents_skill_dirs(&nested);
    assert_eq!(dirs, [nested.join(".agents/skills"), repo.join("a/.agents/skills"), repo.join(".agents/skills")]);

    let agents = repo.join(".agents/skills");
    write(&agents.join("root.md"), &skill_md("root", "Not loaded in agents mode"));
    write(&agents.join("group/loose.md"), &skill_md("loose", "Loaded below the root"));
    write(&agents.join("tool/SKILL.md"), &skill_md("tool", "Tool"));
    let found = collect_skill_entries(&agents, SkillDiscoveryMode::Agents);
    assert_eq!(found, [agents.join("group/loose.md"), agents.join("tool/SKILL.md")]);
    let pi_mode = collect_skill_entries(&agents, SkillDiscoveryMode::Pi);
    assert_eq!(pi_mode, [agents.join("root.md"), agents.join("tool/SKILL.md")]);
}

struct Layout {
    _temp: tempfile::TempDir,
    home: PathBuf,
    agent: PathBuf,
    cwd: PathBuf,
}

fn layout() -> Layout {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let agent = home.join(".pi/agent");
    let cwd = temp.path().join("work/project");
    std::fs::create_dir_all(temp.path().join("work/.git")).unwrap();
    write(&agent.join("skills/user-skill/SKILL.md"), &skill_md("user-skill", "User"));
    write(&agent.join("skills/skip-skill/SKILL.md"), &skill_md("skip-skill", "Skip me"));
    write(&agent.join("prompts/user.md"), "User prompt");
    write(&agent.join("prompts/skip.md"), "Skip prompt");
    write(&agent.join("prompts/nested/deep.md"), "Not auto-discovered");
    write(&agent.join("themes/user.json"), "{}");
    write(&agent.join("extensions/disabled.ts"), "");
    write(&agent.join("extensions/tool/index.ts"), "");
    write(&home.join(".agents/skills/home-skill/SKILL.md"), &skill_md("home-skill", "Home"));
    write(&cwd.join(".pi/skills/project-skill/SKILL.md"), &skill_md("project-skill", "Project"));
    write(&cwd.join(".pi/prompts/project.md"), "Project prompt");
    write(&cwd.join(".agents/skills/agents-skill/SKILL.md"), &skill_md("agents-skill", "Agents"));
    write(&cwd.join("../.agents/skills/repo-skill/SKILL.md"), &skill_md("repo-skill", "Repo"));
    write(&cwd.join("extra/prompts/a/extra.md"), "Extra");
    Layout { _temp: temp, home, agent, cwd }
}

fn resolve(layout: &Layout, trusted: bool, user: &ResourceSettings, project: &ResourceSettings) -> ResolvedPaths {
    let builtins = strings(&["mcp"]);
    let options = LocalResourceOptions {
        cwd: &layout.cwd,
        agent_dir: &layout.agent,
        home: &layout.home,
        project_trusted: trusted,
        user,
        project,
        builtin_extensions: &builtins,
    };
    resolve_local_resources(&options, &ResolvedPaths::default())
}

#[test]
fn resolves_auto_discovered_resources_with_overrides() {
    let layout = layout();
    let user = ResourceSettings {
        extensions: strings(&["-extensions/disabled.ts", "-builtin:mcp"]),
        skills: strings(&["-skills/skip-skill"]),
        prompts: strings(&["-prompts/skip.md"]),
        themes: Vec::new(),
    };
    let project = ResourceSettings { prompts: strings(&["../extra/prompts"]), ..Default::default() };
    let resolved = resolve(&layout, true, &user, &project);
    assert_eq!(
        enabled_names(&resolved.skills),
        ["SKILL.md"; 5],
        "project, agents, repo, user, and home skills: {:?}",
        resolved.skills
    );
    let skill_dirs: Vec<String> = resolved
        .skills
        .iter()
        .map(|r| r.path.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(skill_dirs, ["project-skill", "agents-skill", "repo-skill", "skip-skill", "user-skill", "home-skill"]);
    assert!(!resolved.skills[3].enabled);
    assert_eq!(resolved.skills[1].metadata.base_dir.as_deref(), Some(layout.cwd.join(".agents").as_path()));
    assert_eq!(resolved.skills[5].metadata.scope, SourceScope::User);
    assert_eq!(enabled_names(&resolved.prompts), ["extra.md", "project.md", "user.md"]);
    assert_eq!(resolved.prompts[0].metadata.source, "local");
    assert_eq!(enabled_names(&resolved.themes), ["user.json"]);
    assert_eq!(enabled_names(&resolved.extensions), ["index.ts"]);
    let builtin = resolved.extensions.iter().find(|r| r.path == Path::new("builtin:mcp")).unwrap();
    assert!(!builtin.enabled);

    let project_enables = ResourceSettings { extensions: strings(&["+builtin:mcp"]), ..Default::default() };
    let resolved = resolve(&layout, true, &user, &project_enables);
    let builtin = resolved.extensions.iter().find(|r| r.path == Path::new("builtin:mcp")).unwrap();
    assert!(builtin.enabled);
    assert_eq!(builtin.metadata.scope, SourceScope::Project);
}

#[test]
fn untrusted_projects_contribute_nothing() {
    let layout = layout();
    let project = ResourceSettings { prompts: strings(&["../extra/prompts"]), ..Default::default() };
    let resolved = resolve(&layout, false, &ResourceSettings::default(), &project);
    let all: Vec<&ResolvedResource> =
        resolved.skills.iter().chain(&resolved.prompts).chain(&resolved.themes).chain(&resolved.extensions).collect();
    assert!(all.iter().all(|resource| resource.metadata.scope != SourceScope::Project), "{all:?}");
    assert!(all.iter().all(|resource| !resource.path.starts_with(&layout.cwd)));
    assert!(all.iter().all(|resource| !resource.path.to_string_lossy().contains("repo-skill")));
    assert!(resolved.skills.iter().any(|r| r.path.to_string_lossy().contains("home-skill")));
}
