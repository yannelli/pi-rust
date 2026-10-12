//! Port of upstream `test/skills.test.ts`.

use std::path::{Path, PathBuf};

use super::{skill_fixtures, skill_md, write};
use crate::resources::skills::{
    LoadSkillsOptions, SkillFileReader, expand_skill_command, format_skills_for_prompt, load_skills,
    load_skills_from_dir, parse_skill_block,
};
use crate::resources::source_info::{SourceScope, create_synthetic_source_info};
use crate::resources::{DiagnosticKind, ResourceDiagnostic, Skill};

fn has(diagnostics: &[ResourceDiagnostic], text: &str) -> bool {
    diagnostics.iter().any(|diagnostic| diagnostic.message.contains(text))
}

fn test_skill(name: &str, description: &str, path: &str, hidden: bool) -> Skill {
    let file_path = PathBuf::from(path);
    Skill {
        name: name.into(),
        description: description.into(),
        base_dir: file_path.parent().unwrap().to_path_buf(),
        source_info: create_synthetic_source_info(path, "test", None, None),
        file_path,
        disable_model_invocation: hidden,
        license: None,
        compatibility: None,
        metadata: Default::default(),
        allowed_tools: Vec::new(),
    }
}

#[test]
fn loads_fixture_skills_with_validation_warnings() {
    let temp = tempfile::tempdir().unwrap();
    let fixtures = skill_fixtures(temp.path());
    let load = |name: &str| load_skills_from_dir(&fixtures.join(name), "test");

    let valid = load("valid-skill");
    assert_eq!(valid.skills.len(), 1);
    assert_eq!(valid.skills[0].name, "valid-skill");
    assert_eq!(valid.skills[0].description, "A valid skill for testing purposes.");
    assert_eq!(valid.skills[0].source_info.source, "test");
    assert!(!valid.skills[0].disable_model_invocation);
    assert!(valid.diagnostics.is_empty());

    let mismatch = load("name-mismatch");
    assert_eq!(mismatch.skills[0].name, "different-name");
    assert!(!has(&mismatch.diagnostics, "does not match parent directory"));

    let invalid = load("invalid-name-chars");
    assert_eq!(invalid.skills.len(), 1);
    assert!(has(&invalid.diagnostics, "invalid characters"));

    let long = load("long-name");
    assert_eq!(long.skills.len(), 1);
    assert!(has(&long.diagnostics, "exceeds 64 characters"));

    let hyphens = load("consecutive-hyphens");
    assert_eq!(hyphens.skills.len(), 1);
    assert!(has(&hyphens.diagnostics, "consecutive hyphens"));

    for name in ["missing-description", "no-frontmatter"] {
        let result = load(name);
        assert!(result.skills.is_empty(), "{name}");
        assert!(has(&result.diagnostics, "description is required"), "{name}");
    }

    let unknown = load("unknown-field");
    assert_eq!(unknown.skills.len(), 1);
    assert!(unknown.diagnostics.is_empty());

    let yaml = load("invalid-yaml");
    assert!(yaml.skills.is_empty());
    assert!(has(&yaml.diagnostics, "at line"), "{:?}", yaml.diagnostics);

    let multiline = load("multiline-description");
    assert!(multiline.skills[0].description.contains('\n'));
    assert!(multiline.skills[0].description.contains("This is a multiline description."));
    assert!(multiline.diagnostics.is_empty());

    let hidden = load("disable-model-invocation");
    assert!(hidden.skills[0].disable_model_invocation);
    assert!(!has(&hidden.diagnostics, "unknown frontmatter field"));
}

#[test]
fn discovers_nested_skills_and_prefers_root_skill_files() {
    let temp = tempfile::tempdir().unwrap();
    let fixtures = skill_fixtures(temp.path());
    let nested = load_skills_from_dir(&fixtures.join("nested"), "test");
    assert_eq!(nested.skills.len(), 1);
    assert_eq!(nested.skills[0].name, "child-skill");
    assert!(nested.diagnostics.is_empty());

    let root = load_skills_from_dir(&fixtures.join("root-skill-preferred"), "test");
    assert_eq!(root.skills.len(), 1);
    assert_eq!(root.skills[0].name, "root-skill-preferred");
    assert_eq!(root.skills[0].description, "Root skill should win.");

    let all = load_skills_from_dir(&fixtures, "test");
    assert!(all.skills.len() >= 6);
    let missing = load_skills_from_dir(Path::new("/non/existent/path"), "test");
    assert!(missing.skills.is_empty() && missing.diagnostics.is_empty());
}

#[test]
fn standalone_markdown_needs_a_description_and_ignore_files_apply() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("skills");
    write(&dir.join("standalone.md"), &skill_md("standalone", "Loose skill"));
    write(&dir.join("notes.md"), "No frontmatter here");
    write(&dir.join("ignored/SKILL.md"), &skill_md("ignored", "Ignored skill"));
    write(&dir.join("deep/inner/extra.md"), &skill_md("extra", "Not a root file"));
    write(&dir.join(".gitignore"), "ignored/\n");
    let result = load_skills_from_dir(&dir, "user");
    let names: Vec<&str> = result.skills.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(names, ["standalone"]);
    assert!(result.diagnostics.is_empty());
    assert_eq!(result.skills[0].source_info.scope, SourceScope::User);
    assert_eq!(result.skills[0].source_info.source, "local");
}

#[test]
fn reads_optional_spec_fields() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("pdf-tools/SKILL.md");
    write(
        &file,
        "---\nname: pdf-tools\ndescription: PDFs.\nlicense: MIT\ncompatibility: Needs poppler\nmetadata:\n  author: a\nallowed-tools: Bash(git:*) Read\n---\nBody",
    );
    let skill = load_skills_from_dir(&temp.path().join("pdf-tools"), "test").skills.remove(0);
    assert_eq!(skill.license.as_deref(), Some("MIT"));
    assert_eq!(skill.compatibility.as_deref(), Some("Needs poppler"));
    assert_eq!(skill.metadata["author"], "a");
    assert_eq!(skill.allowed_tools, ["Bash(git:*)", "Read"]);
    assert_eq!(skill.base_dir, temp.path().join("pdf-tools"));
}

#[test]
fn formats_skills_for_the_prompt() {
    assert_eq!(format_skills_for_prompt(&[], SkillFileReader::Read), "");
    let skills = [
        test_skill("skill-one", "A skill with <special> & \"characters\".", "/path/one/SKILL.md", false),
        test_skill("skill-two", "Second skill.", "/path/two/SKILL.md", false),
        test_skill("hidden-skill", "A hidden skill.", "/path/hidden/SKILL.md", true),
    ];
    let result = format_skills_for_prompt(&skills, SkillFileReader::Read);
    let intro = &result[..result.find("<available_skills>").unwrap()];
    assert!(intro.contains("The following skills provide specialized instructions"));
    assert!(intro.contains("Use the read tool to load a skill's file"));
    assert!(result.contains("<name>skill-one</name>") && result.contains("<name>skill-two</name>"));
    assert!(result.contains("<location>/path/one/SKILL.md</location>"));
    assert!(
        result.contains("&lt;special&gt;") && result.contains("&amp;") && result.contains("&quot;characters&quot;")
    );
    assert!(!result.contains("hidden-skill"));
    assert_eq!(result.matches("<skill>").count(), 2);
    assert!(result.ends_with("</available_skills>"));
    assert_eq!(format_skills_for_prompt(&skills[2..], SkillFileReader::Read), "");
    assert!(format_skills_for_prompt(&skills, SkillFileReader::Bash).contains("Use bash to load"));
    assert!(format_skills_for_prompt(&skills, SkillFileReader::Indirect).contains("\nLoad a skill's file"));
}

#[test]
fn load_skills_handles_explicit_paths_and_collisions() {
    let temp = tempfile::tempdir().unwrap();
    let fixtures = skill_fixtures(temp.path());
    let (agent, cwd) = (temp.path().join("empty-agent"), temp.path().join("empty-cwd"));
    let options = |paths: Vec<String>| LoadSkillsOptions {
        cwd: cwd.clone(),
        agent_dir: agent.clone(),
        skill_paths: paths,
        include_defaults: true,
    };
    let explicit = load_skills(&options(vec![fixtures.join("valid-skill").display().to_string()]));
    assert_eq!(explicit.skills.len(), 1);
    assert_eq!(explicit.skills[0].source_info.scope, SourceScope::Temporary);
    assert!(explicit.diagnostics.is_empty());

    let missing = load_skills(&options(vec!["/non/existent/path".into()]));
    assert!(missing.skills.is_empty());
    assert!(has(&missing.diagnostics, "does not exist"));

    write(&temp.path().join("first/calendar/SKILL.md"), &skill_md("calendar", "First calendar skill."));
    write(&temp.path().join("second/calendar/SKILL.md"), &skill_md("calendar", "Second calendar skill."));
    let paths = ["first", "second"].map(|dir| temp.path().join(dir).display().to_string()).to_vec();
    let collided = load_skills(&options(paths));
    assert_eq!(collided.skills.len(), 1);
    assert_eq!(collided.skills[0].description, "First calendar skill.");
    let collision = collided.diagnostics.iter().find(|d| d.kind == DiagnosticKind::Collision).unwrap();
    assert_eq!(collision.message, "name \"calendar\" collision");
    let detail = collision.collision.as_ref().unwrap();
    assert!(detail.winner_path.contains("first") && detail.loser_path.contains("second"));

    let same_file = fixtures.join("valid-skill/SKILL.md").display().to_string();
    let twice = load_skills(&options(vec![same_file.clone(), same_file]));
    assert_eq!(twice.skills.len(), 1);
    assert!(twice.diagnostics.is_empty());
}

#[test]
fn expands_skill_commands_and_parses_blocks() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("pdf/SKILL.md");
    write(&file, "---\nname: pdf\ndescription: PDFs.\n---\n\nRead `references/formats.md`.\n");
    let skills = load_skills_from_dir(&temp.path().join("pdf"), "test").skills;
    let expanded = expand_skill_command("/skill:pdf extract report.pdf ", &skills).unwrap();
    let base = temp.path().join("pdf");
    let block = format!(
        "<skill name=\"pdf\" location=\"{}\">\nReferences are relative to {}.\n\nRead `references/formats.md`.\n</skill>",
        file.display(),
        base.display()
    );
    assert_eq!(expanded, format!("{block}\n\nextract report.pdf"));
    assert_eq!(expand_skill_command("/skill:pdf", &skills).unwrap(), block);
    assert_eq!(expand_skill_command("/skill:other x", &skills).unwrap(), "/skill:other x");
    assert_eq!(expand_skill_command("hello", &skills).unwrap(), "hello");

    let parsed = parse_skill_block(&expanded).unwrap();
    assert_eq!(parsed.name, "pdf");
    assert_eq!(parsed.location, file.display().to_string());
    assert!(parsed.content.starts_with("References are relative to"));
    assert_eq!(parsed.user_message.as_deref(), Some("extract report.pdf"));
    assert_eq!(parse_skill_block(&block).unwrap().user_message, None);
    assert!(parse_skill_block("plain text").is_none());

    std::fs::remove_file(&file).unwrap();
    assert!(expand_skill_command("/skill:pdf", &skills).is_err());
}
