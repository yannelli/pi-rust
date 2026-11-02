use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pi_js::{HostValue, JsError};
use serde_json::{Value, json};

use super::ops::{Args, error};
use super::ops_sdk_storage::StorageState;
use super::runner::ExtensionRunner;
use crate::agent_session::{AgentSession, lock};

mod child;

pub(crate) struct ChildSession {
    pub session: Arc<AgentSession>,
    pub manager_id: u64,
    /// The child host of the `DefaultResourceLoader` extensions the session bound.
    pub extensions: Option<super::ops_sdk_resources::Hosted>,
}

#[derive(Default)]
pub(crate) struct SdkState {
    pub storage: Mutex<StorageState>,
    pub children: Mutex<HashMap<u64, ChildSession>>,
    subscriptions: Mutex<HashMap<u64, child::Subscription>>,
}

impl SdkState {
    pub fn shutdown(&self) {
        for (_, subscription) in lock(&self.subscriptions).drain() {
            subscription.stop(None);
        }
        let children: Vec<_> = lock(&self.children).drain().map(|(_, child)| child).collect();
        for child in children {
            child.session.dispose();
            lock(&self.storage).detach_child(child.manager_id, &child.session);
        }
    }

    fn session(&self, id: u64) -> Result<Arc<AgentSession>, JsError> {
        lock(&self.children)
            .get(&id)
            .map(|child| child.session.clone())
            .ok_or_else(|| error("AgentSession has been disposed"))
    }
}

impl Drop for SdkState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) fn sync(runner: &ExtensionRunner, name: &str, args: &Args) -> Result<HostValue, JsError> {
    let value = match name {
        "renderDiff" => json!(crate::interactive::components::diff::render_diff(&args.str(0)?)),
        "loadSkills" => {
            let options = args.json(0);
            let skill_paths = options
                .get("skillPaths")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(error)?
                .unwrap_or_default();
            let result = crate::resources::skills::load_skills(&crate::resources::skills::LoadSkillsOptions {
                cwd: options["cwd"].as_str().map(Into::into).unwrap_or_else(|| runner.cwd().to_owned()),
                agent_dir: options["agentDir"]
                    .as_str()
                    .map(Into::into)
                    .unwrap_or_else(|| runner.agent_dir().to_owned()),
                skill_paths,
                include_defaults: options["includeDefaults"].as_bool().unwrap_or(false),
            });
            json!({"skills": result.skills, "diagnostics": result.diagnostics})
        }
        "loadSkillsFromDir" => {
            let options = args.json(0);
            let dir = options["dir"].as_str().ok_or_else(|| error("Missing skills directory"))?;
            let source = options["source"].as_str().ok_or_else(|| error("Missing skills source"))?;
            let result = crate::resources::skills::load_skills_from_dir(std::path::Path::new(dir), source);
            json!({"skills": result.skills, "diagnostics": result.diagnostics})
        }
        "generateDiffString" => {
            let (diff, first_changed_line) =
                crate::tools::edit_diff::generate_diff_string(&args.str(0)?, &args.str(1)?);
            json!({"diff": diff, "firstChangedLine": first_changed_line})
        }
        "generateUnifiedPatch" => {
            json!(crate::tools::edit_diff::generate_unified_patch(&args.str(0)?, &args.str(1)?, &args.str(2)?))
        }
        "dispose" => {
            let removed = lock(&runner.sdk.children).remove(&args.u64(0));
            if let Some(child) = removed {
                child::unsubscribe(&runner.sdk, args.u64(0), Some(&child.session));
                child.session.dispose();
                lock(&runner.sdk.storage).detach_child(child.manager_id, &child.session);
            }
            Value::Null
        }
        "read" => child::read(runner.sdk.session(args.u64(0))?.as_ref(), &args.str(1)?)?,
        "control" => child::control(runner, &runner.sdk.session(args.u64(0))?, args)?,
        _ => return Err(JsError::unsupported(&format!("AgentSession.{name}"))),
    };
    Ok(HostValue::Json(value))
}

pub(crate) async fn run(runner: Arc<ExtensionRunner>, name: &str, args: Args) -> Result<HostValue, JsError> {
    let value = match name {
        "create" => super::ops_sdk_create::create(&runner, args.json(0)).await?,
        "convertToPng" => {
            let (data, mime_type) = (args.str(0)?, args.str(1)?);
            tokio::task::spawn_blocking(move || {
                crate::images::convert_to_png(&data, &mime_type)
                    .map(|(data, mime_type)| json!({"data": data, "mimeType": mime_type}))
                    .unwrap_or(Value::Null)
            })
            .await
            .map_err(error)?
        }
        "resizeImage" => {
            let bytes: Vec<u8> = serde_json::from_value(args.json(0)).map_err(error)?;
            let mime_type = args.str(1)?;
            let options: Option<pi_ai::ImageResizeOptions> = serde_json::from_value(args.json(2)).map_err(error)?;
            tokio::task::spawn_blocking(move || {
                crate::images::resize_image(&bytes, &mime_type, options.as_ref())
                    .map(|image| {
                        json!({
                            "data": image.data,
                            "mimeType": image.mime_type,
                            "originalWidth": image.original_width,
                            "originalHeight": image.original_height,
                            "width": image.width,
                            "height": image.height,
                            "wasResized": image.was_resized,
                        })
                    })
                    .unwrap_or(Value::Null)
            })
            .await
            .map_err(error)?
        }
        "prompt" | "steer" | "followUp" | "waitForIdle" | "setModel" | "compact" | "abort" => {
            child::run(&runner, name, &args).await?
        }
        _ => return Err(JsError::unsupported(&format!("AgentSession.{name}"))),
    };
    Ok(HostValue::Json(value))
}
