use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pi_js::{HostValue, JsError};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::ops::{Args, error};
use super::ops_sdk_storage::StorageState;
use super::runner::ExtensionRunner;
use crate::agent_session::{AgentSession, PromptOptions, lock};

pub(crate) struct ChildSession {
    pub session: Arc<AgentSession>,
    pub manager_id: u64,
}

#[derive(Default)]
pub(crate) struct SdkState {
    pub storage: Mutex<StorageState>,
    pub children: Mutex<HashMap<u64, ChildSession>>,
}

impl SdkState {
    pub fn shutdown(&self) {
        for (_, child) in lock(&self.children).drain() {
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
            if let Some(child) = lock(&runner.sdk.children).remove(&args.u64(0)) {
                child.session.dispose();
                lock(&runner.sdk.storage).detach_child(child.manager_id, &child.session);
            }
            Value::Null
        }
        "read" => {
            let session = runner.sdk.session(args.u64(0))?;
            match args.str(1)?.as_str() {
                "messages" => json!(session.messages()),
                "model" => json!(session.model()),
                "thinkingLevel" => json!(session.thinking_level()),
                "isStreaming" => json!(session.is_streaming()),
                "sessionId" => json!(session.session_id()),
                "sessionFile" => json!(session.session_file()),
                "systemPrompt" => json!(session.agent().state().current_system_prompt()),
                "routedModel" => json!(
                    session
                        .routed_model()
                        .map(|routed| { json!({"model": routed.model, "thinkingLevel": routed.thinking_level}) })
                ),
                "contextUsage" => json!(session.context_usage().map(|usage| {
                    json!({"tokens": usage.tokens, "contextWindow": usage.context_window, "percent": usage.percent})
                })),
                member => return Err(JsError::unsupported(&format!("AgentSession.{member}"))),
            }
        }
        _ => return Err(JsError::unsupported(&format!("AgentSession.{name}"))),
    };
    Ok(HostValue::Json(value))
}

async fn prompt(runner: &ExtensionRunner, args: &Args) -> Result<(), JsError> {
    let session = runner.sdk.session(args.u64(0))?;
    let text = args.str(1)?;
    let options = args.json(2);
    let callback = args.u64(3);
    let images = options.get("images").cloned().map(serde_json::from_value).transpose().map_err(error)?;
    let prompt_options = PromptOptions {
        images: images.unwrap_or_default(),
        expand_prompt_templates: options["expandPromptTemplates"].as_bool().unwrap_or(true),
        source: options["source"].as_str().map(str::to_owned),
        ..Default::default()
    };
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let listener = session.subscribe(move |event| {
        let _ = sender.send(event.to_json());
    });
    let run = async {
        let result = session.prompt_with(&text, prompt_options).await;
        session.unsubscribe(listener);
        result.map_err(error)
    };
    let forward = async {
        let mut failure = None;
        while let Some(event) = receiver.recv().await {
            if let Err(error) =
                runner.host().call_method("invokeCallback", vec![json!(callback).into(), event.into()]).await
            {
                session.request_abort();
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    };
    let (result, delivered) = tokio::join!(run, forward);
    result?;
    delivered
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
        "prompt" => {
            prompt(&runner, &args).await?;
            Value::Null
        }
        "abort" => {
            runner.sdk.session(args.u64(0))?.abort().await;
            Value::Null
        }
        _ => return Err(JsError::unsupported(&format!("AgentSession.{name}"))),
    };
    Ok(HostValue::Json(value))
}
