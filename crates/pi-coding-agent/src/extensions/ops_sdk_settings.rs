//! `storage.settings.*`: upstream `SettingsManager` for SDK use. Rust holds each manager and runs
//! its members; `sdk_settings.js` runs custom `withLock` storage and the write queue.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_js::JsError;
use serde_json::{Map, Value, json};

use super::ops::{Args, error};
use super::ops_sdk_storage::StorageState;
use super::runner::ExtensionRunner;
use crate::session::ids::resolve_path_from;
use crate::settings::{FileSettingsStorage, HostStorage, Read, SettingsManager, SettingsScope, Thrown};

fn scope(name: &str) -> Result<SettingsScope, JsError> {
    match name {
        "global" => Ok(SettingsScope::Global),
        "project" => Ok(SettingsScope::Project),
        other => Err(error(format!("Unknown settings scope: {other}"))),
    }
}

/// `options.projectTrusted ?? true`.
fn trusted(options: &Value) -> bool {
    match &options["projectTrusted"] {
        Value::Null => true,
        Value::Bool(flag) => *flag,
        other => !matches!(other, Value::String(text) if text.is_empty()) && other.as_f64() != Some(0.0),
    }
}

/// A JavaScript read: `{ content }`, or `{ thrown: { token, message } }` when `withLock` threw.
fn read(value: Value) -> Read {
    match value.get("thrown") {
        Some(thrown) => Err(Thrown {
            message: thrown["message"].as_str().unwrap_or_default().to_string(),
            token: thrown["token"].as_u64(),
        }),
        None => Ok(value.get("content").cloned().filter(|content| !content.is_null())),
    }
}

fn optional_read(value: Value) -> Option<Read> {
    (!value.is_null()).then(|| read(value))
}

/// Upstream `resolvePath`: `~` expands, relative paths resolve against `base`.
fn resolve(path: &str, base: &Path, env: &Value) -> PathBuf {
    let home = env["HOME"].as_str().map(PathBuf::from).unwrap_or_default();
    let path = match path.strip_prefix('~') {
        Some("") => home,
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    };
    resolve_path_from(&path, base)
}

fn insert(state: &mut StorageState, manager: SettingsManager) -> Value {
    json!(state.insert_settings(Arc::new(manager)))
}

/// Wake the JavaScript queue through callback `key` when a child session queues a write.
fn watch(runner: &ExtensionRunner, manager: &SettingsManager, key: u64) {
    let host = runner.host().clone();
    manager.sdk_watch(Arc::new(move || {
        let host = host.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = host.call_method("invokeCallback", vec![json!(key).into()]).await;
            });
        }
    }));
}

fn errors(manager: &SettingsManager) -> Value {
    let errors = manager.sdk_drain_errors().into_iter().map(|(failure, token)| {
        let path = failure.path.as_ref().map(|path| path.display().to_string());
        json!({"scope": failure.scope.as_str(), "path": path, "message": failure.message, "token": token})
    });
    Value::Array(errors.collect())
}

pub(super) fn settings_op(
    runner: &ExtensionRunner,
    state: &mut StorageState,
    name: &str,
    args: &Args,
) -> Result<Value, JsError> {
    match name {
        "create" => {
            let env = args.json(3);
            let cwd =
                args.opt_str(0).map_or_else(|| runner.cwd().to_path_buf(), |cwd| resolve(&cwd, runner.cwd(), &env));
            let agent_dir = match args.opt_str(1).or_else(|| env["PI_CODING_AGENT_DIR"].as_str().map(String::from)) {
                Some(dir) if !dir.is_empty() => resolve(&dir, runner.cwd(), &env),
                _ => runner.agent_dir().to_path_buf(),
            };
            let storage = Arc::new(FileSettingsStorage::new(&cwd, &agent_dir));
            let manager = SettingsManager::sdk(storage, trusted(&args.json(2)), None);
            return Ok(insert(state, manager));
        }
        "inMemory" => {
            let manager = SettingsManager::sdk_in_memory(args.json(0), trusted(&args.json(1))).map_err(error)?;
            return Ok(insert(state, manager));
        }
        "fromStorage" => {
            let reads = (read(args.json(1)), optional_read(args.json(2)));
            let manager = SettingsManager::sdk(Arc::new(HostStorage), trusted(&args.json(0)), Some(reads));
            return Ok(insert(state, manager));
        }
        "dispose" => {
            state.remove_settings(args.u64(0));
            return Ok(Value::Null);
        }
        _ => {}
    }
    let manager = state.settings(args.u64(0)).ok_or_else(|| error("SettingsManager has been disposed"))?;
    Ok(match name {
        "watch" => {
            watch(runner, &manager, args.u64(1));
            Value::Null
        }
        "call" => {
            let method = args.str(1)?;
            let call_args = args.json(2).as_array().cloned().unwrap_or_default();
            let env = args.json(3).as_object().cloned().unwrap_or_else(Map::new);
            let (value, alias) = manager
                .sdk_call(&method, &call_args, &env)
                .map_err(error)?
                .ok_or_else(|| JsError::unsupported(&format!("SettingsManager.{method}")))?;
            json!({"value": value, "queued": manager.sdk_claim(), "alias": alias})
        }
        "claim" => json!(manager.sdk_claim()),
        "alias" => {
            manager.sdk_alias(&args.json(1), args.json(2));
            Value::Null
        }
        "persist" => {
            manager.sdk_persist();
            Value::Null
        }
        "persistTake" => match manager.sdk_take() {
            Some((scope, refused)) => json!({"scope": scope.as_str(), "error": refused}),
            None => Value::Null,
        },
        "persistText" => json!(manager.sdk_text(Some(args.json(1))).map_err(error)?),
        "persistDone" => {
            manager.sdk_done(scope(&args.str(1)?)?);
            Value::Null
        }
        "record" => {
            let thrown = Thrown { message: args.opt_str(3).unwrap_or_default(), token: args.json(2).as_u64() };
            manager.sdk_record(scope(&args.str(1)?)?, thrown);
            Value::Null
        }
        "reload" => {
            let reads = optional_read(args.json(1)).map(|global| (global, optional_read(args.json(2))));
            manager.sdk_reload(reads);
            Value::Null
        }
        "setProjectTrusted" => {
            manager.sdk_set_trusted(args.bool(1), optional_read(args.json(2)));
            Value::Null
        }
        "drainErrors" => errors(&manager),
        other => return Err(JsError::unsupported(&format!("SettingsManager.{other}"))),
    })
}
