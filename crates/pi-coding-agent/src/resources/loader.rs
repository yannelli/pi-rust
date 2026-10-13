//! The resource loader: everything upstream `DefaultResourceLoader` collects except loading
//! extension code. Settings values and CLI flags arrive as plain options.

use std::path::{Path, PathBuf};

use super::brand::CONFIG_DIR_NAME;
use super::context::{ContextFile, load_project_context_files};
use super::discovery::{LocalResourceOptions, ResourceSettings, resolve_local_resources};
use super::paths::{absolute, file_kind, home_dir, is_local_path, sorted_entries};
use super::prompts::{LoadPromptTemplatesOptions, PromptTemplate, load_prompt_templates};
use super::skills::{LoadSkillsOptions, Skill, load_skills};
use super::source_info::{
    BUILTIN_PATH_PREFIX, PathMetadata, ResolvedPaths, ResolvedResource, SourceInfo, SourceOrigin, SourceScope,
};
use super::sources::{SourceLookup, merge_paths, resolve_extension_sources, resolve_resource_path};
use super::{CollisionResourceType, ResourceDiagnostic};

#[derive(Clone, Debug)]
pub struct ResourceLoaderOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    /// Home directory for `~/.agents/skills`.
    pub home: PathBuf,
    /// Whether protected project resources load. Resolve it with [`super::trust`] first.
    pub project_trusted: bool,
    pub user_settings: ResourceSettings,
    pub project_settings: ResourceSettings,
    /// Resources of `packages` settings, resolved by the package manager.
    pub package_resources: ResolvedPaths,
    /// `-e` sources; package sources among them arrive resolved in `extension_source_resources`.
    pub extension_sources: Vec<String>,
    pub extension_source_resources: ResolvedPaths,
    /// `--skill`, `--prompt-template`, and `--theme` paths.
    pub skill_paths: Vec<String>,
    pub prompt_template_paths: Vec<String>,
    pub theme_paths: Vec<String>,
    /// Built-in extension names (listed as `builtin:<name>`) and the ones to leave out.
    pub builtin_extensions: Vec<String>,
    pub disabled_builtin_extensions: Vec<String>,
    pub no_extensions: bool,
    pub no_skills: bool,
    pub no_prompt_templates: bool,
    pub no_themes: bool,
    pub no_context_files: bool,
    /// `--system-prompt`: a file path or literal text; replaces `SYSTEM.md` discovery.
    pub system_prompt: Option<String>,
    /// `--append-system-prompt` values: file paths or literal text; replace `APPEND_SYSTEM.md`.
    pub append_system_prompt: Option<Vec<String>>,
}

impl ResourceLoaderOptions {
    /// Defaults for `cwd` and `agent_dir`. Like upstream, the project starts trusted.
    pub fn new(cwd: &Path, agent_dir: &Path) -> Self {
        Self {
            cwd: absolute(cwd),
            agent_dir: absolute(agent_dir),
            home: home_dir(),
            project_trusted: true,
            user_settings: ResourceSettings::default(),
            project_settings: ResourceSettings::default(),
            package_resources: ResolvedPaths::default(),
            extension_sources: Vec::new(),
            extension_source_resources: ResolvedPaths::default(),
            skill_paths: Vec::new(),
            prompt_template_paths: Vec::new(),
            theme_paths: Vec::new(),
            builtin_extensions: Vec::new(),
            disabled_builtin_extensions: Vec::new(),
            no_extensions: false,
            no_skills: false,
            no_prompt_templates: false,
            no_themes: false,
            no_context_files: false,
            system_prompt: None,
            append_system_prompt: None,
        }
    }
}

/// An extension to load: a file, a directory, or a `builtin:<name>` path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionEntry {
    pub path: PathBuf,
    pub source_info: SourceInfo,
}

/// A theme JSON file; the theme loader validates it. `name` is its `name` field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeFile {
    pub path: PathBuf,
    pub name: Option<String>,
    pub source_info: SourceInfo,
}

/// Resource paths that extensions contribute after a reload (`resources_discover`).
#[derive(Clone, Debug, Default)]
pub struct ExtensionResourcePaths {
    pub skill_paths: Vec<(PathBuf, PathMetadata)>,
    pub prompt_paths: Vec<(PathBuf, PathMetadata)>,
    pub theme_paths: Vec<(PathBuf, PathMetadata)>,
}

#[derive(Clone, Debug, Default)]
pub struct LoadedResources {
    pub extensions: Vec<ExtensionEntry>,
    pub extension_diagnostics: Vec<ResourceDiagnostic>,
    pub skills: Vec<Skill>,
    pub skill_diagnostics: Vec<ResourceDiagnostic>,
    pub prompts: Vec<PromptTemplate>,
    pub prompt_diagnostics: Vec<ResourceDiagnostic>,
    pub themes: Vec<ThemeFile>,
    pub theme_diagnostics: Vec<ResourceDiagnostic>,
    pub context_files: Vec<ContextFile>,
    pub system_prompt: Option<String>,
    pub system_prompt_source: Option<PathBuf>,
    pub append_system_prompt: Vec<String>,
    pub append_system_prompt_sources: Vec<PathBuf>,
}

impl LoadedResources {
    /// `APPEND_SYSTEM.md` texts joined the way the system prompt addendum uses them.
    pub fn append_system_prompt_text(&self) -> String {
        self.append_system_prompt.join("\n\n")
    }
}

pub struct ResourceLoader {
    pub options: ResourceLoaderOptions,
    lookup: SourceLookup,
    extra: [Vec<(PathBuf, SourceInfo)>; 3],
    last_paths: [Vec<PathBuf>; 3],
    loaded: LoadedResources,
}

const SKILLS: usize = 0;
const PROMPTS: usize = 1;
const THEMES: usize = 2;

fn enabled(resources: &[ResolvedResource]) -> Vec<PathBuf> {
    resources.iter().filter(|resource| resource.enabled).map(|resource| resource.path.clone()).collect()
}

/// A settings-named or package skill directory holding `SKILL.md` loads as that file.
fn map_skill_path(resource: &ResolvedResource, lookup: &mut SourceLookup) -> PathBuf {
    let metadata = &resource.metadata;
    let skill_file = resource.path.join("SKILL.md");
    if (metadata.source == "auto" || metadata.origin == SourceOrigin::Package)
        && resource.path.is_dir()
        && skill_file.exists()
    {
        lookup.record(&skill_file, metadata);
        return skill_file;
    }
    resource.path.clone()
}

fn resolve_prompt_input(input: &str, description: &str) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    if !Path::new(input).exists() {
        return Some(input.to_string());
    }
    match std::fs::read_to_string(input) {
        Ok(text) => Some(text.trim_start_matches('\u{feff}').to_string()),
        Err(error) => {
            eprintln!("Warning: Could not read {description} file {input}: {error}");
            Some(input.to_string())
        }
    }
}

impl ResourceLoader {
    pub fn new(options: ResourceLoaderOptions) -> Self {
        let lookup = SourceLookup::new(&options.cwd, &options.agent_dir);
        Self {
            options,
            lookup,
            extra: Default::default(),
            last_paths: Default::default(),
            loaded: LoadedResources::default(),
        }
    }

    pub fn resources(&self) -> &LoadedResources {
        &self.loaded
    }

    pub fn set_project_trusted(&mut self, trusted: bool) {
        self.options.project_trusted = trusted;
    }

    /// Discover and load every resource for the current options.
    pub fn reload(&mut self) {
        crate::diagnostics::reset_timings(crate::diagnostics::timings::EXTENSIONS);
        let options = &self.options;
        let local = LocalResourceOptions {
            cwd: &options.cwd,
            agent_dir: &options.agent_dir,
            home: &options.home,
            project_trusted: options.project_trusted,
            user: &options.user_settings,
            project: &options.project_settings,
            builtin_extensions: &options.builtin_extensions,
        };
        let resolved = resolve_local_resources(&local, &options.package_resources);
        let cli =
            resolve_extension_sources(&options.extension_sources, &options.cwd, &options.extension_source_resources);
        let mut lookup = SourceLookup::new(&options.cwd, &options.agent_dir);
        self.extra = Default::default();
        for list in [&resolved.extensions, &resolved.skills, &resolved.prompts, &resolved.themes] {
            list.iter().for_each(|resource| lookup.record(&resource.path, &resource.metadata));
        }
        let skill_resources: Vec<ResolvedResource> = resolved.skills.iter().filter(|r| r.enabled).cloned().collect();
        let enabled_skills: Vec<PathBuf> = skill_resources.iter().map(|r| map_skill_path(r, &mut lookup)).collect();
        let cli_metadata = PathMetadata::new("cli", SourceScope::Temporary, SourceOrigin::TopLevel);
        for resource in cli.extensions.iter().chain(&cli.skills) {
            lookup.record(&resource.path, &cli_metadata);
        }
        for resource in cli.prompts.iter().chain(&cli.themes) {
            lookup.record(&resource.path, &resource.metadata);
        }

        let mut extension_paths = enabled(&cli.extensions);
        if !options.no_extensions {
            extension_paths.extend(enabled(&resolved.extensions));
        }
        let disabled = &options.disabled_builtin_extensions;
        let extension_paths: Vec<PathBuf> = merge_paths(&options.cwd, &extension_paths, &[])
            .into_iter()
            .filter(|path| {
                let text = path.to_string_lossy();
                text.strip_prefix(BUILTIN_PATH_PREFIX).is_none_or(|name| !disabled.iter().any(|d| d == name))
            })
            .collect();
        let mut extension_diagnostics = Vec::new();
        for source in options.extension_sources.iter().filter(|source| is_local_path(source)) {
            let path = resolve_resource_path(&options.cwd, source);
            if !path.exists() {
                let message = format!("Extension path does not exist: {}", path.display());
                extension_diagnostics.push(ResourceDiagnostic::error(message, &path));
            }
        }
        let extensions = extension_paths
            .into_iter()
            .map(|path| ExtensionEntry { source_info: lookup.info_for(&path, &[]), path })
            .collect();
        self.lookup = lookup;
        self.loaded.extensions = extensions;
        self.loaded.extension_diagnostics = extension_diagnostics;

        let options = &self.options;
        let pick = |cli: &[ResolvedResource], discovered: Vec<PathBuf>, off: bool| {
            let mut primary = enabled(cli);
            if !off {
                primary.extend(discovered);
            }
            primary
        };
        let skills = pick(&cli.skills, enabled_skills, options.no_skills);
        let prompts = pick(&cli.prompts, enabled(&resolved.prompts), options.no_prompt_templates);
        let themes = pick(&cli.themes, enabled(&resolved.themes), options.no_themes);
        self.last_paths[SKILLS] = merge_paths(&options.cwd, &skills, &options.skill_paths);
        self.last_paths[PROMPTS] = merge_paths(&options.cwd, &prompts, &options.prompt_template_paths);
        self.last_paths[THEMES] = merge_paths(&options.cwd, &themes, &options.theme_paths);
        self.update_skills();
        self.update_prompts();
        self.update_themes();
        self.report_missing();
        self.load_prompt_files();
    }

    fn report_missing(&mut self) {
        let options = &self.options;
        let checks = [
            (&options.skill_paths, &mut self.loaded.skill_diagnostics, "Skill path does not exist", true),
            (
                &options.prompt_template_paths,
                &mut self.loaded.prompt_diagnostics,
                "Prompt template path does not exist",
                true,
            ),
            (&options.theme_paths, &mut self.loaded.theme_diagnostics, "Theme path does not exist", false),
        ];
        for (paths, diagnostics, message, local_only) in checks {
            for raw in paths.iter().filter(|raw| !local_only || is_local_path(raw)) {
                let path = resolve_resource_path(&options.cwd, raw);
                if !path.exists() && !diagnostics.iter().any(|diagnostic| diagnostic.path_is(&path)) {
                    diagnostics.push(ResourceDiagnostic::error(message, &path));
                }
            }
        }
    }

    fn load_prompt_files(&mut self) {
        let options = &self.options;
        self.loaded.context_files = if options.no_context_files {
            Vec::new()
        } else {
            load_project_context_files(&options.cwd, &options.agent_dir)
        };
        let discover = |name: &str| -> Option<String> {
            let project = options.cwd.join(CONFIG_DIR_NAME).join(name);
            if options.project_trusted && project.exists() {
                return Some(project.display().to_string());
            }
            let global = options.agent_dir.join(name);
            global.exists().then(|| global.display().to_string())
        };
        let source = options.system_prompt.clone().or_else(|| discover("SYSTEM.md"));
        self.loaded.system_prompt = source.as_deref().and_then(|source| resolve_prompt_input(source, "system prompt"));
        self.loaded.system_prompt_source =
            source.filter(|source| Path::new(source).exists()).map(|source| absolute(Path::new(&source)));
        let append =
            options.append_system_prompt.clone().unwrap_or_else(|| discover("APPEND_SYSTEM.md").into_iter().collect());
        self.loaded.append_system_prompt =
            append.iter().filter_map(|source| resolve_prompt_input(source, "append system prompt")).collect();
        self.loaded.append_system_prompt_sources = append
            .iter()
            .filter(|source| Path::new(source).exists())
            .map(|source| absolute(Path::new(source)))
            .collect();
    }

    /// Add skill, prompt, and theme paths contributed by extensions and reload those types.
    pub fn extend_resources(&mut self, paths: ExtensionResourcePaths) {
        let cwd = self.options.cwd.clone();
        let lists = [(SKILLS, paths.skill_paths), (PROMPTS, paths.prompt_paths), (THEMES, paths.theme_paths)];
        for (index, entries) in lists {
            if entries.is_empty() {
                continue;
            }
            let mut added = Vec::new();
            for (path, mut metadata) in entries {
                let path = resolve_resource_path(&cwd, &path.to_string_lossy());
                metadata.base_dir = metadata.base_dir.map(|dir| resolve_resource_path(&cwd, &dir.to_string_lossy()));
                let info = super::source_info::create_source_info(&path.display().to_string(), &metadata);
                self.extra[index].push((path.clone(), info));
                added.push(path.display().to_string());
            }
            self.last_paths[index] = merge_paths(&cwd, &self.last_paths[index], &added);
            match index {
                SKILLS => self.update_skills(),
                PROMPTS => self.update_prompts(),
                _ => self.update_themes(),
            }
        }
    }

    fn update_skills(&mut self) {
        let paths = &self.last_paths[SKILLS];
        if self.options.no_skills && paths.is_empty() {
            (self.loaded.skills, self.loaded.skill_diagnostics) = (Vec::new(), Vec::new());
            return;
        }
        let result = load_skills(&LoadSkillsOptions {
            cwd: self.options.cwd.clone(),
            agent_dir: self.options.agent_dir.clone(),
            skill_paths: paths.iter().map(|path| path.display().to_string()).collect(),
            include_defaults: false,
        });
        let lookup = &self.lookup;
        let extra = &self.extra[SKILLS];
        self.loaded.skills = result
            .skills
            .into_iter()
            .map(|skill| Skill {
                source_info: lookup.find(&skill.file_path, extra).unwrap_or(skill.source_info.clone()),
                ..skill
            })
            .collect();
        self.loaded.skill_diagnostics = result.diagnostics;
    }

    fn update_prompts(&mut self) {
        let paths = &self.last_paths[PROMPTS];
        if self.options.no_prompt_templates && paths.is_empty() {
            (self.loaded.prompts, self.loaded.prompt_diagnostics) = (Vec::new(), Vec::new());
            return;
        }
        let result = load_prompt_templates(&LoadPromptTemplatesOptions {
            cwd: self.options.cwd.clone(),
            agent_dir: self.options.agent_dir.clone(),
            prompt_paths: paths.iter().map(|path| path.display().to_string()).collect(),
            include_defaults: false,
        });
        let mut diagnostics = result.diagnostics;
        let mut prompts: Vec<PromptTemplate> = Vec::new();
        for prompt in result.templates {
            match prompts.iter().find(|existing| existing.name == prompt.name) {
                Some(existing) => diagnostics.push(ResourceDiagnostic::collision(
                    CollisionResourceType::Prompt,
                    format!("name \"/{}\" collision", prompt.name),
                    &prompt.name,
                    &existing.file_path,
                    &prompt.file_path,
                )),
                None => prompts.push(prompt),
            }
        }
        let (lookup, extra) = (&self.lookup, &self.extra[PROMPTS]);
        for prompt in &mut prompts {
            if let Some(info) = lookup.find(&prompt.file_path, extra) {
                prompt.source_info = info;
            }
        }
        (self.loaded.prompts, self.loaded.prompt_diagnostics) = (prompts, diagnostics);
    }

    fn update_themes(&mut self) {
        let paths = self.last_paths[THEMES].clone();
        let mut diagnostics = Vec::new();
        let mut themes: Vec<ThemeFile> = Vec::new();
        if !(self.options.no_themes && paths.is_empty()) {
            for path in &paths {
                match file_kind(path) {
                    None => diagnostics.push(ResourceDiagnostic::warning("theme path does not exist", path)),
                    Some((_, true)) => {
                        for (name, file) in sorted_entries(path) {
                            if name.ends_with(".json") && file_kind(&file).is_some_and(|(is_file, _)| is_file) {
                                self.load_theme(&file, &mut themes, &mut diagnostics);
                            }
                        }
                    }
                    Some((true, _)) if path.extension().is_some_and(|ext| ext == "json") => {
                        self.load_theme(path, &mut themes, &mut diagnostics)
                    }
                    Some(_) => diagnostics.push(ResourceDiagnostic::warning("theme path is not a json file", path)),
                }
            }
        }
        (self.loaded.themes, self.loaded.theme_diagnostics) = (themes, diagnostics);
    }

    fn load_theme(&self, path: &Path, themes: &mut Vec<ThemeFile>, diagnostics: &mut Vec<ResourceDiagnostic>) {
        let parsed = std::fs::read_to_string(path).map_err(|error| error.to_string()).and_then(|text| {
            serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}'))
                .map_err(|error| error.to_string())
        });
        let value = match parsed {
            Ok(value) => value,
            Err(error) => return diagnostics.push(ResourceDiagnostic::warning(error, path)),
        };
        let name = value.get("name").and_then(serde_json::Value::as_str).map(String::from);
        let key = name.clone().unwrap_or_else(|| "unnamed".into());
        if let Some(winner) = themes.iter().find(|theme| theme.name.clone().unwrap_or_else(|| "unnamed".into()) == key)
        {
            let message = format!("name \"{key}\" collision");
            diagnostics.push(ResourceDiagnostic::collision(
                CollisionResourceType::Theme,
                message,
                &key,
                &winner.path,
                path,
            ));
            return;
        }
        let source_info = self.lookup.info_for(path, &self.extra[THEMES]);
        themes.push(ThemeFile { path: path.to_path_buf(), name, source_info });
    }
}
