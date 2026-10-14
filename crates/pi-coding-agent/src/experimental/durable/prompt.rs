//! Port of upstream `durable/prompt.ts`: pi's system prompt sections as one Harness extension.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pi_durable::harness::{Extension, PromptInput, section};

use crate::config::ContextFile;
use crate::resources::Skill;
use crate::resources::context::load_project_context_files;
use crate::resources::skills::{LoadSkillsOptions, load_skills};
use crate::settings::{ResourceKind, SettingsManager};
use crate::system_prompt::{BuildSystemPromptOptions, build_system_prompt_sections};

/// pi's section order; `build_system_prompt_sections` omits the ones without content.
const KEYS: [&str; 7] = ["preamble", "tools", "rules", "docs", "project_context", "skills", "cwd"];

/// The tools pi's prompt describes from their built-in snippets and guidelines.
const CONTRIBUTIONS: [&str; 4] = ["read", "bash", "edit", "write"];

type Resources = (Vec<ContextFile>, Vec<Skill>);
type Sections = Arc<HashMap<String, String>>;

struct PiPrompt {
    settings: Arc<SettingsManager>,
    agent_dir: PathBuf,
    fallback_cwd: String,
    resources: Mutex<HashMap<String, Arc<Resources>>>,
    /// Upstream builds once per request input; here a build is kept per directory and tool list.
    built: Mutex<HashMap<(String, Vec<String>), Sections>>,
}

impl PiPrompt {
    fn load(&self, cwd: &str) -> Arc<Resources> {
        let mut resources = self.resources.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        resources
            .entry(cwd.to_string())
            .or_insert_with(|| {
                let path = PathBuf::from(cwd);
                let files = load_project_context_files(&path, &self.agent_dir);
                let skills = load_skills(&LoadSkillsOptions {
                    cwd: path,
                    agent_dir: self.agent_dir.clone(),
                    skill_paths: self.settings.merged_resource_paths(ResourceKind::Skills),
                    include_defaults: true,
                });
                Arc::new((files, skills.skills))
            })
            .clone()
    }

    fn build(&self, input: &PromptInput) -> Sections {
        let env_cwd = input.env.as_ref().map(|env| env.cwd());
        let cwd = env_cwd.or_else(|| input.agent.cwd.clone()).unwrap_or_else(|| self.fallback_cwd.clone());
        let tools = input.agent.tool_names();
        let key = (cwd.clone(), tools.clone());
        if let Some(found) = self.built.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&key) {
            return found.clone();
        }
        let resources = self.load(&cwd);
        let mut options = BuildSystemPromptOptions::new(&PathBuf::from(&cwd));
        options.selected_tools = tools.iter().filter(|name| CONTRIBUTIONS.contains(&name.as_str())).cloned().collect();
        options = options.with_builtin_tool_info();
        options.selected_tools = tools;
        options.context_files = resources.0.clone();
        options.skills = resources.1.clone();
        let sections = build_system_prompt_sections(&options).unwrap_or_default();
        let built = Arc::new(sections.into_iter().collect::<HashMap<_, _>>());
        self.built.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(key, built.clone());
        built
    }
}

/// pi's system prompt as one extension: the sections of `build_system_prompt_sections` for the request's
/// tools and the conversation's directory. Context files and skills load once per directory.
pub fn create_pi_prompt(settings: Arc<SettingsManager>, agent_dir: PathBuf, fallback_cwd: String) -> Extension {
    let prompt =
        Arc::new(PiPrompt { settings, agent_dir, fallback_cwd, resources: Mutex::default(), built: Mutex::default() });
    let mut extension = Extension::new("pi-prompt");
    for key in KEYS {
        let prompt = prompt.clone();
        // The built sections carry their own tags.
        extension = extension.section(section(
            key,
            move |input, _| {
                let text = prompt.build(&input).get(key).cloned();
                async move { Ok(text) }
            },
            Some(false),
        ));
    }
    extension
}
