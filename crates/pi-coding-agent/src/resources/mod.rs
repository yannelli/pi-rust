//! Resources: context files, skills, prompt templates, theme and extension paths, system
//! prompt files, and project trust. Ported from upstream `core/resource-loader.ts`,
//! `skills.ts`, `prompt-templates.ts`, `trust-manager.ts`, `project-trust.ts`, `source-info.ts`.

pub mod brand;
pub mod context;
pub mod discovery;
pub mod filters;
pub mod frontmatter;
pub mod loader;
pub mod paths;
pub mod prompts;
pub mod skills;
pub mod source_info;
pub mod sources;
pub mod trust;

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

pub use context::{ContextFile, load_project_context_files};
pub use discovery::{ResourceSettings, ResourceType};
pub use loader::{
    ExtensionEntry, ExtensionResourcePaths, LoadedResources, ResourceLoader, ResourceLoaderOptions, ThemeFile,
};
pub use prompts::{PromptTemplate, expand_prompt_template, parse_command_args, substitute_args};
pub use skills::{ParsedSkillBlock, Skill, expand_skill_command, format_skills_for_prompt, parse_skill_block};
pub use source_info::{PathMetadata, ResolvedPaths, ResolvedResource, SourceInfo, SourceOrigin, SourceScope};
pub use trust::{
    DefaultProjectTrust, ProjectTrustOption, ProjectTrustRequest, ProjectTrustResolution, ProjectTrustStore,
    has_trust_requiring_project_resources,
};

/// Expand user input the way upstream does before it reaches the agent: `/skill:name args`,
/// then `/template args`. On an error, report it and send the original text.
pub fn expand_user_input(text: &str, skills: &[Skill], templates: &[PromptTemplate]) -> anyhow::Result<String> {
    Ok(expand_prompt_template(&expand_skill_command(text, skills)?, templates))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticKind {
    Warning,
    Error,
    Collision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CollisionResourceType {
    Extension,
    Skill,
    Prompt,
    Theme,
}

/// Two resources with the same name: the first one loaded wins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceCollision {
    pub resource_type: CollisionResourceType,
    pub name: String,
    pub winner_path: String,
    pub loser_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loser_source: Option<String>,
}

/// A problem found while loading resources. Loading continues past every diagnostic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDiagnostic {
    #[serde(rename = "type")]
    pub kind: DiagnosticKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collision: Option<ResourceCollision>,
}

impl ResourceDiagnostic {
    pub fn warning(message: impl Into<String>, path: impl AsRef<std::path::Path>) -> Self {
        Self::new(DiagnosticKind::Warning, message, path)
    }

    pub fn error(message: impl Into<String>, path: impl AsRef<std::path::Path>) -> Self {
        Self::new(DiagnosticKind::Error, message, path)
    }

    fn new(kind: DiagnosticKind, message: impl Into<String>, path: impl AsRef<std::path::Path>) -> Self {
        Self { kind, message: message.into(), path: Some(path.as_ref().display().to_string()), collision: None }
    }

    pub fn collision(
        resource_type: CollisionResourceType,
        message: impl Into<String>,
        name: &str,
        winner: &std::path::Path,
        loser: &std::path::Path,
    ) -> Self {
        Self {
            kind: DiagnosticKind::Collision,
            message: message.into(),
            path: Some(loser.display().to_string()),
            collision: Some(ResourceCollision {
                resource_type,
                name: name.to_string(),
                winner_path: winner.display().to_string(),
                loser_path: loser.display().to_string(),
                winner_source: None,
                loser_source: None,
            }),
        }
    }

    pub fn path_is(&self, path: &std::path::Path) -> bool {
        self.path.as_deref().is_some_and(|own| std::path::Path::new(own) == path)
    }
}
