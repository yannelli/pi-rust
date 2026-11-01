//! `resources.load` and `resources.set` for each kind, in the order upstream `reload` runs them.

use std::path::{Path, PathBuf};

use pi_js::JsError;
use pi_tui::colors::TerminalColorMode;
use pi_tui::terminal_image::{detect_capabilities_with, get_terminal_color_mode};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::super::ops::error;
use super::super::ops_host::theme_snapshot;
use super::typed;
use crate::resources::loader::resolve_prompt_input;
use crate::resources::paths::{file_kind, sorted_entries};
use crate::resources::{
    CollisionResourceType, ContextFile, PromptTemplate, ResourceDiagnostic, ResourceLoader, ResourceType, Skill,
    SourceInfo, ThemeFile,
};
use crate::settings::SettingsManager;
use crate::theme::{Theme, load_theme_from_path};

/// Upstream `findSourceInfoForPath ?? given ?? default`. `None` tells JavaScript to keep the
/// object the override gave, so its identity survives as upstream.
fn derive(loader: &ResourceLoader, kind: ResourceType, path: &str, given: &Value) -> Option<SourceInfo> {
    match loader.find_source_info(kind, Path::new(path)) {
        Some(info) => Some(info),
        None if !given.is_null() => None,
        None => Some(loader.default_source_info(Path::new(path))),
    }
}

fn set_items<T: DeserializeOwned>(
    loader: &ResourceLoader,
    kind: ResourceType,
    items: Value,
    member: &str,
) -> Result<(Vec<T>, Vec<Value>), JsError> {
    let (mut stored, mut infos) = (Vec::new(), Vec::new());
    for mut item in typed::<Vec<Value>>(items, member)? {
        if !item.is_object() {
            return Err(JsError::type_error(format!("{member}: each entry must be an object")));
        }
        let derived = derive(loader, kind, item["filePath"].as_str().unwrap_or_default(), &item["sourceInfo"]);
        if let Some(info) = &derived {
            item["sourceInfo"] = json!(info);
        }
        stored.push(typed(item, member)?);
        infos.push(json!(derived));
    }
    Ok((stored, infos))
}

fn set_themes(loader: &ResourceLoader, items: Value, member: &str) -> Result<(Vec<ThemeFile>, Vec<Value>), JsError> {
    let (mut files, mut infos) = (Vec::new(), Vec::new());
    for item in typed::<Vec<Value>>(items, member)? {
        let path = item["sourcePath"].as_str().filter(|path| !path.is_empty());
        let derived = path.and_then(|path| derive(loader, ResourceType::Themes, path, &item["sourceInfo"]));
        if let Some(path) = path {
            let source_info = match &derived {
                Some(info) => info.clone(),
                None => typed(item["sourceInfo"].clone(), member)?,
            };
            let name = item["name"].as_str().map(String::from);
            files.push(ThemeFile { path: PathBuf::from(path), name, source_info });
        }
        infos.push(json!(derived));
    }
    Ok((files, infos))
}

fn color_mode(settings: &SettingsManager) -> TerminalColorMode {
    let mut capabilities = detect_capabilities_with(&pi_tui::env::ProcessEnv, &|| false);
    if let Some(true_color) = settings.terminal_capability_overrides().1 {
        capabilities.true_color = true_color;
    }
    get_terminal_color_mode(&capabilities)
}

fn load_themes(loader: &ResourceLoader, mode: TerminalColorMode) -> (Vec<Theme>, Vec<ResourceDiagnostic>) {
    let (mut themes, mut diagnostics) = (Vec::new(), Vec::new());
    let paths = loader.paths(ResourceType::Themes);
    if loader.options.no_themes && paths.is_empty() {
        return (themes, diagnostics);
    }
    let mut load = |file: &Path, diagnostics: &mut Vec<ResourceDiagnostic>| {
        let loaded = load_theme_from_path(file, Some(mode));
        match loaded {
            Ok(theme) => themes.push(theme),
            Err(failure) => diagnostics.push(ResourceDiagnostic::warning(failure.0, file)),
        }
    };
    for path in paths {
        let json = path.to_string_lossy().ends_with(".json");
        match file_kind(path) {
            None => diagnostics.push(ResourceDiagnostic::warning("theme path does not exist", path)),
            Some((_, true)) => {
                for (name, file) in sorted_entries(path) {
                    if name.ends_with(".json") && file_kind(&file).is_some_and(|(is_file, _)| is_file) {
                        load(&file, &mut diagnostics);
                    }
                }
            }
            Some((true, _)) if json => load(path, &mut diagnostics),
            Some(_) => diagnostics.push(ResourceDiagnostic::warning("theme path is not a json file", path)),
        }
    }
    let (mut kept, builtin) = (Vec::<Theme>::new(), Path::new("<builtin>"));
    for theme in themes {
        let name = theme.name().unwrap_or("unnamed").to_string();
        match kept.iter().find(|winner| winner.name().unwrap_or("unnamed") == name) {
            Some(winner) => diagnostics.push(ResourceDiagnostic::collision(
                CollisionResourceType::Theme,
                format!("name \"{name}\" collision"),
                &name,
                winner.source_path().unwrap_or(builtin),
                theme.source_path().unwrap_or(builtin),
            )),
            None => kept.push(theme),
        }
    }
    (kept, diagnostics)
}

pub(super) fn load(loader: &mut ResourceLoader, settings: &SettingsManager, kind: &str) -> Result<Value, JsError> {
    Ok(match kind {
        "skills" => {
            let (skills, diagnostics) = loader.base_skills();
            json!({"skills": skills, "diagnostics": diagnostics})
        }
        "prompts" => {
            let (prompts, diagnostics) = loader.base_prompts();
            json!({"prompts": prompts, "diagnostics": diagnostics})
        }
        "themes" => {
            let (themes, diagnostics) = load_themes(loader, color_mode(settings));
            json!({"themes": themes.iter().map(theme_snapshot).collect::<Vec<_>>(), "diagnostics": diagnostics})
        }
        "agentsFiles" => {
            let files = loader.base_context_files().into_iter();
            let files: Vec<Value> = files.map(|file| json!({"path": file.path, "content": file.content})).collect();
            json!({ "agentsFiles": files })
        }
        "systemPrompt" | "appendSystemPrompt" => {
            loader.set_project_trusted(settings.is_project_trusted());
            let (inputs, description) = match kind {
                "systemPrompt" => (loader.system_prompt_input().into_iter().collect(), "system prompt"),
                _ => (loader.append_system_prompt_inputs(), "append system prompt"),
            };
            let texts = inputs.iter().filter_map(|input| resolve_prompt_input(input, description));
            let texts: Vec<String> = texts.collect();
            let value = if kind == "systemPrompt" { json!(texts.first()) } else { json!(texts) };
            json!({"value": value, "inputs": inputs})
        }
        other => return Err(error(format!("Unknown resource kind: {other}"))),
    })
}

pub(super) fn set(
    loader: &mut ResourceLoader,
    kind: &str,
    mut value: Value,
    report: bool,
    inputs: Value,
) -> Result<Value, JsError> {
    let member = format!("DefaultResourceLoader.{kind}Override");
    let inputs = || typed::<Vec<String>>(inputs, &member);
    let diagnostics = |value: &mut Value| typed::<Vec<ResourceDiagnostic>>(value["diagnostics"].take(), &member);
    let (resource, infos) = match kind {
        "skills" => {
            let (skills, infos) = set_items::<Skill>(loader, ResourceType::Skills, value["skills"].take(), &member)?;
            loader.set_skills(skills, diagnostics(&mut value)?);
            (ResourceType::Skills, infos)
        }
        "prompts" => {
            let items = value["prompts"].take();
            let (prompts, infos) = set_items::<PromptTemplate>(loader, ResourceType::Prompts, items, &member)?;
            loader.set_prompts(prompts, diagnostics(&mut value)?);
            (ResourceType::Prompts, infos)
        }
        "themes" => {
            let (themes, infos) = set_themes(loader, value["themes"].take(), &member)?;
            loader.set_themes(themes, diagnostics(&mut value)?);
            (ResourceType::Themes, infos)
        }
        "agentsFiles" => {
            let files = typed::<Vec<Value>>(value, &member)?.into_iter().map(|file| {
                match (file["path"].as_str(), file["content"].as_str()) {
                    (Some(path), Some(content)) => Ok(ContextFile { path: path.into(), content: content.into() }),
                    _ => Err(JsError::type_error(format!("{member}: a file needs a string path and content"))),
                }
            });
            loader.set_context_files(files.collect::<Result<_, _>>()?);
            return Ok(Value::Null);
        }
        "systemPrompt" => {
            loader.set_system_prompt(typed(value, &member)?, inputs()?.first().map(String::as_str));
            return Ok(json!(loader.resources().system_prompt_source));
        }
        "appendSystemPrompt" => {
            loader.set_append_system_prompt(typed(value, &member)?, &inputs()?);
            return Ok(json!(loader.resources().append_system_prompt_sources));
        }
        other => return Err(error(format!("Unknown resource kind: {other}"))),
    };
    let missing = if report { loader.report_missing_for(resource) } else { Vec::new() };
    Ok(json!({"sourceInfos": infos, "missing": missing}))
}
