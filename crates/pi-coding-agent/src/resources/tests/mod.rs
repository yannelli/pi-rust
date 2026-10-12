//! Tests ported from upstream `coding-agent/test`: `skills.test.ts`, `prompt-templates.test.ts`,
//! `trust-manager.test.ts`, `resource-loader.test.ts`, and `system-prompt.test.ts`.

mod context;
mod discovery;
mod loader;
mod prompts;
mod skills;
mod system_prompt;
mod trust;

use std::path::Path;

/// Write `content` to `path`, creating parent directories.
pub fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

pub fn skill_md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n")
}

/// The upstream `test/fixtures/skills` tree.
pub const SKILL_FIXTURES: &[(&str, &str)] = &[
    (
        "consecutive-hyphens/SKILL.md",
        "---\nname: bad--name\ndescription: A skill with consecutive hyphens in the name.\n---\n\n# Consecutive Hyphens\n",
    ),
    (
        "disable-model-invocation/SKILL.md",
        "---\nname: disable-model-invocation\ndescription: A skill that cannot be invoked by the model.\ndisable-model-invocation: true\n---\n\n# Manual Only Skill\n",
    ),
    (
        "invalid-name-chars/SKILL.md",
        "---\nname: Invalid_Name\ndescription: A skill with invalid characters in the name.\n---\n\n# Invalid Name\n",
    ),
    ("invalid-yaml/SKILL.md", "---\nname: invalid-yaml\ndescription: [unclosed bracket\n---\n\n# Invalid YAML Skill\n"),
    (
        "long-name/SKILL.md",
        "---\nname: this-is-a-very-long-skill-name-that-exceeds-the-sixty-four-character-limit-set-by-the-standard\ndescription: A skill with a name that exceeds 64 characters.\n---\n\n# Long Name\n",
    ),
    ("missing-description/SKILL.md", "---\nname: missing-description\n---\n\n# Missing Description\n"),
    (
        "multiline-description/SKILL.md",
        "---\nname: multiline-description\ndescription: |\n  This is a multiline description.\n  It spans multiple lines.\n  And should be normalized.\n---\n\n# Multiline Description Skill\n",
    ),
    (
        "name-mismatch/SKILL.md",
        "---\nname: different-name\ndescription: A skill with a name that doesn't match the directory.\n---\n\n# Name Mismatch\n",
    ),
    (
        "nested/child-skill/SKILL.md",
        "---\nname: child-skill\ndescription: A nested skill in a subdirectory.\n---\n\n# Child Skill\n",
    ),
    ("no-frontmatter/SKILL.md", "# No Frontmatter\n\nThis skill has no YAML frontmatter at all.\n"),
    ("root-skill-preferred/SKILL.md", "---\ndescription: Root skill should win.\n---\n"),
    ("root-skill-preferred/nested-child/SKILL.md", "---\ndescription: Nested skill should be ignored.\n---\n"),
    (
        "unknown-field/SKILL.md",
        "---\nname: unknown-field\ndescription: A skill with an unknown frontmatter field.\nauthor: someone\nversion: 1.0\n---\n\n# Unknown Field\n",
    ),
    (
        "valid-skill/SKILL.md",
        "---\nname: valid-skill\ndescription: A valid skill for testing purposes.\n---\n\n# Valid Skill\n\nThis is a valid skill that follows the Agent Skills standard.\n",
    ),
];

pub fn skill_fixtures(root: &Path) -> std::path::PathBuf {
    let dir = root.join("skills");
    for (path, content) in SKILL_FIXTURES {
        write(&dir.join(path), content);
    }
    dir
}
