//! `storage.settings.*`: upstream `SettingsManager` over the Rust settings module. Typed
//! getters return upstream defaults; reads and writes go through the lock-protected files.

use std::sync::Arc;

use pi_js::JsError;
use serde_json::{Value, json};

use super::ops::{Args, error};
use super::ops_sdk_storage::StorageState;
use super::runner::ExtensionRunner;
use crate::settings::{ResourceKind, SettingsManager, SettingsOptions, SettingsScope};

fn thinking_level(args: &Args, index: usize) -> Result<pi_ai::ThinkingLevel, JsError> {
    crate::model_resolver::parse_thinking_level(&args.str(index)?).ok_or_else(|| error("Invalid thinking level"))
}

fn strings(args: &Args, index: usize) -> Result<Vec<String>, JsError> {
    serde_json::from_value(args.json(index)).map_err(error)
}

fn resource_kind(method: &str) -> Option<ResourceKind> {
    let name = method.trim_start_matches("get").trim_start_matches("set").trim_start_matches("Project");
    match name {
        "ExtensionPaths" => Some(ResourceKind::Extensions),
        "SkillPaths" => Some(ResourceKind::Skills),
        "PromptTemplatePaths" => Some(ResourceKind::Prompts),
        "ThemePaths" => Some(ResourceKind::Themes),
        _ => None,
    }
}

fn path_json(path: Option<std::path::PathBuf>) -> Value {
    json!(path.map(|path| path.display().to_string()))
}

/// Typed `SettingsManager` getters and setters; method arguments start at 2.
fn call(manager: &SettingsManager, method: &str, args: &Args) -> Result<Value, JsError> {
    let done = Value::Null;
    if let Some(kind) = resource_kind(method) {
        return match method.get(..3) {
            Some("get") => Ok(json!(manager.merged_resource_paths(kind))),
            _ if method.starts_with("setProject") => {
                manager.set_project_resource_paths(kind, &strings(args, 2)?).map(|_| done).map_err(error)
            }
            _ => {
                manager.set_resource_paths(kind, &strings(args, 2)?);
                Ok(done)
            }
        };
    }
    let model = || serde_json::from_value::<pi_ai::Model>(args.json(2)).ok();
    Ok(match method {
        "getLastChangelogVersion" => json!(manager.last_changelog_version()),
        "getSessionDir" => path_json(manager.session_dir()),
        "getDefaultProvider" => json!(manager.default_provider()),
        "getDefaultModel" => json!(manager.default_model()),
        "getThemeSetting" => json!(manager.theme_setting()),
        "getTheme" => json!(manager.theme()),
        "getDefaultThinkingLevel" => json!(manager.default_thinking_level().map(|level| level.as_str())),
        "getModelThinkingLevel" => {
            json!(manager.model_thinking_level(&args.str(2)?, &args.str(3)?).map(|level| level.as_str()))
        }
        "getAllModelThinkingLevels" => {
            let levels = manager.all_model_thinking_levels();
            levels
                .into_iter()
                .map(|(key, level)| (key, json!(level.as_str())))
                .collect::<serde_json::Map<_, _>>()
                .into()
        }
        "getTransport" => json!(manager.transport()),
        "getCompactionEnabled" => json!(manager.compaction_enabled()),
        "getCompactionReserveTokens" => json!(manager.compaction_settings(model().as_ref()).reserve_tokens),
        "getCompactionKeepRecentTokens" => json!(manager.compaction_settings(model().as_ref()).keep_recent_tokens),
        "getRetryEnabled" => json!(manager.retry_enabled()),
        "getHideThinkingBlock" => json!(manager.hide_thinking_block()),
        "getSteeringMode" => json!(manager.steering_mode()),
        "getFollowUpMode" => json!(manager.follow_up_mode()),
        "getExternalEditorCommand" => json!(manager.external_editor_command()),
        "getShellPath" => path_json(manager.shell_path()),
        "getShellCommandPrefix" => json!(manager.shell_command_prefix()),
        "getDefaultProjectTrust" => json!(manager.default_project_trust()),
        "getNpmCommand" => json!(manager.npm_command()),
        "getEnableSkillCommands" => json!(manager.enable_skill_commands()),
        "getEnabledModels" => json!(manager.enabled_models()),
        "getDefaultTools" => json!(manager.default_tools()),
        "getImageWidthCells" => json!(manager.image_width_cells()),
        "getClearOnShrink" => json!(manager.clear_on_shrink()),
        "getEnableAnalytics" => json!(manager.enable_analytics()),
        "getHttpIdleTimeoutMs" => json!(manager.http_idle_timeout_ms().map_err(error)?),
        "setLastChangelogVersion" => {
            manager.set_last_changelog_version(&args.str(2)?);
            done
        }
        "setDefaultProvider" => {
            manager.set_default_provider(&args.str(2)?);
            done
        }
        "setDefaultModel" => {
            manager.set_default_model(&args.str(2)?);
            done
        }
        "setDefaultModelAndProvider" => {
            manager.set_default_model_and_provider(&args.str(2)?, &args.str(3)?);
            done
        }
        "setTheme" => {
            manager.set_theme(&args.str(2)?);
            done
        }
        "setDefaultThinkingLevel" => {
            manager.set_default_thinking_level(thinking_level(args, 2)?);
            done
        }
        "setModelThinkingLevel" => {
            manager.set_model_thinking_level(&args.str(2)?, &args.str(3)?, thinking_level(args, 4)?);
            done
        }
        "removeModelThinkingLevel" => {
            manager.remove_model_thinking_level(&args.str(2)?, &args.str(3)?);
            done
        }
        "setCompactionEnabled" => {
            manager.set_compaction_enabled(args.bool(2));
            done
        }
        "setRetryEnabled" => {
            manager.set_retry_enabled(args.bool(2));
            done
        }
        "setHideThinkingBlock" => {
            manager.set_hide_thinking_block(args.bool(2));
            done
        }
        "setShellPath" => {
            manager.set_shell_path(args.opt_str(2).as_deref());
            done
        }
        "setShellCommandPrefix" => {
            manager.set_shell_command_prefix(args.opt_str(2).as_deref());
            done
        }
        "setEnabledModels" => {
            let patterns: Option<Vec<String>> = serde_json::from_value(args.json(2)).map_err(error)?;
            manager.set_enabled_models(patterns.as_deref());
            done
        }
        "setImageWidthCells" => {
            manager.set_image_width_cells(args.u64(2));
            done
        }
        "setClearOnShrink" => {
            manager.set_clear_on_shrink(args.bool(2));
            done
        }
        other => return Err(JsError::unsupported(&format!("SettingsManager.{other}"))),
    })
}

fn scope(name: &str) -> Result<SettingsScope, JsError> {
    match name {
        "global" => Ok(SettingsScope::Global),
        "project" => Ok(SettingsScope::Project),
        other => Err(error(format!("Unknown settings scope: {other}"))),
    }
}

pub(super) fn settings_op(
    runner: &ExtensionRunner,
    state: &mut StorageState,
    name: &str,
    args: &Args,
) -> Result<Value, JsError> {
    let trusted = |index: usize| args.json(index)["projectTrusted"].as_bool().unwrap_or(true);
    match name {
        "create" => {
            let cwd = args.opt_str(0).map(std::path::PathBuf::from).unwrap_or_else(|| runner.cwd().to_path_buf());
            let agent_dir =
                args.opt_str(1).map(std::path::PathBuf::from).unwrap_or_else(|| runner.agent_dir().to_path_buf());
            let manager = SettingsManager::create(&cwd, &agent_dir, SettingsOptions { project_trusted: trusted(2) });
            return Ok(json!(state.insert_settings(Arc::new(manager))));
        }
        "inMemory" => {
            let manager = SettingsManager::in_memory(args.json(0));
            manager.set_project_trusted(trusted(1));
            return Ok(json!(state.insert_settings(Arc::new(manager))));
        }
        "dispose" => {
            state.remove_settings(args.u64(0));
            return Ok(Value::Null);
        }
        _ => {}
    }
    let manager = state.settings(args.u64(0)).ok_or_else(|| error("SettingsManager has been disposed"))?;
    Ok(match name {
        "get" => {
            let settings = match args.opt_str(1).as_deref() {
                Some("global") => manager.global_settings(),
                Some("project") => manager.project_settings(),
                _ => manager.settings(),
            };
            serde_json::to_value(settings).map_err(error)?
        }
        "set" => {
            let value = Some(args.json(3)).filter(|value| !value.is_null());
            match scope(&args.str(1)?)? {
                SettingsScope::Global => manager.set_global_value(&args.str(2)?, value),
                SettingsScope::Project => manager.set_project_value(&args.str(2)?, value).map_err(error)?,
            }
            Value::Null
        }
        "setNested" => {
            let value = Some(args.json(3)).filter(|value| !value.is_null());
            manager.set_global_nested(&args.str(1)?, &args.str(2)?, value);
            Value::Null
        }
        "reload" => {
            manager.reload();
            Value::Null
        }
        "isProjectTrusted" => json!(manager.is_project_trusted()),
        "setProjectTrusted" => {
            manager.set_project_trusted(args.bool(1));
            Value::Null
        }
        "applyOverrides" => {
            manager.apply_overrides(args.json(1));
            Value::Null
        }
        "drainErrors" => manager
            .drain_errors()
            .into_iter()
            .map(|failure| {
                let path = failure.path.map(|path| path.display().to_string());
                json!({"scope": failure.scope.as_str(), "path": path, "message": failure.message})
            })
            .collect(),
        "call" => call(&manager, &args.str(1)?, args)?,
        other => return Err(JsError::unsupported(&format!("SettingsManager.{other}"))),
    })
}
