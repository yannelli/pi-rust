//! Synchronous host operations outside the session: signals, errors, the registry snapshot,
//! themes, configuration, message helpers, built-in tool info, the model catalog, providers,
//! virtual models, MCP servers, and stream sinks.

use pi_js::{HostValue, JsError};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::ops::{Args, error};
use super::runner::{ExtensionError, ExtensionRunner};
use crate::agent_session::lock;
use crate::resources::brand::{APP_NAME, CONFIG_DIR_NAME};
use crate::theme::{ThemeBg, ThemeColor};

/// The ANSI tables of a theme for the JavaScript `Theme`.
fn theme_snapshot(theme: &crate::theme::Theme) -> Value {
    let mut fg = serde_json::Map::new();
    let mut dim = Vec::new();
    for color in ThemeColor::ALL {
        let ansi = theme.get_fg_ansi(*color);
        let ansi = match ansi.strip_suffix("\x1b[2m") {
            Some(base) => {
                dim.push(color.name());
                base.to_string()
            }
            None => ansi,
        };
        fg.insert(color.name().into(), json!(ansi));
    }
    let bg: serde_json::Map<String, Value> =
        ThemeBg::ALL.iter().map(|color| (color.name().into(), json!(theme.get_bg_ansi(*color)))).collect();
    let mode = match theme.get_color_mode() {
        pi_tui::colors::TerminalColorMode::Truecolor => "truecolor",
        pi_tui::colors::TerminalColorMode::Color256 => "256color",
    };
    let appearance = match theme.appearance() {
        crate::theme::ThemeAppearance::Light => "light",
        crate::theme::ThemeAppearance::Dark => "dark",
    };
    json!({
        "name": theme.name(),
        "sourcePath": theme.source_path().map(|path| path.display().to_string()),
        "appearance": appearance,
        "fg": fg,
        "bg": bg,
        "dim": dim,
        "mode": mode,
    })
}

fn builtin_info(runner: &ExtensionRunner, name: &str) -> Result<Value, JsError> {
    let tool = crate::tools::create_tool(name, runner.cwd()).ok_or_else(|| error(format!("Unknown tool: {name}")))?;
    let info = crate::tools::tool_info(name);
    Ok(json!({
        "name": tool.name(),
        "label": tool.label(),
        "description": tool.description(),
        "parameters": tool.parameters(),
        "promptSnippet": info.map(|info| info.snippet),
        "promptGuidelines": info.map(|info| info.guidelines.to_vec()).unwrap_or_default(),
    }))
}

/// Catalog reads; `model` and `models` take a model type, where null matches every type.
fn catalog(kind: &str, args: &Args) -> Value {
    let models = pi_ai::models::builtin_models();
    let of_type = |at: usize| {
        let wanted = args.opt_str(at);
        move |model: &&pi_ai::Model| {
            wanted.as_deref().is_none_or(|wanted| pi_ai::model_operations::model_type(model) == wanted)
        }
    };
    match kind {
        "model" => {
            let (provider, id) = (args.opt_str(1), args.opt_str(2));
            json!(
                models
                    .iter()
                    .filter(of_type(3))
                    .find(|model| Some(&model.provider) == provider.as_ref() && Some(&model.id) == id.as_ref())
            )
        }
        "models" => {
            let provider = args.opt_str(1);
            let models: Vec<_> = models
                .iter()
                .filter(|model| provider.as_ref().is_none_or(|wanted| &model.provider == wanted))
                .filter(of_type(2))
                .collect();
            json!(models)
        }
        "generatedAt" => json!(pi_ai::models::builtin_model_data_generated_at()),
        _ => {
            let mut providers: Vec<&str> = models.iter().map(|model| model.provider.as_str()).collect();
            providers.sort_unstable();
            providers.dedup();
            json!(providers)
        }
    }
}

pub(crate) fn sync(runner: &ExtensionRunner, op: &str, args: &Args) -> Result<HostValue, JsError> {
    let value = match op {
        "signal.create" => {
            let id = runner.next_id();
            let token = CancellationToken::new();
            if args.bool(0) {
                token.cancel();
            }
            lock(&runner.signals).insert(id, token);
            json!(id)
        }
        "oauth.notify" => super::provider::oauth_notify(runner, args)?,
        "signal.abort" => {
            if let Some(token) = lock(&runner.signals).remove(&args.u64(0)) {
                token.cancel();
            }
            Value::Null
        }
        "error" => {
            let record = args.json(0);
            let text = |key: &str| record[key].as_str().unwrap_or_default().to_string();
            runner.emit_error(ExtensionError {
                extension_path: text("extensionPath"),
                event: text("event"),
                error: text("error"),
                stack: record["stack"].as_str().map(str::to_string),
            });
            Value::Null
        }
        "timing" => {
            crate::diagnostics::time_in(&args.str(0)?, crate::diagnostics::timings::EXTENSIONS);
            Value::Null
        }
        "registry" => {
            runner.apply_registry(args.json(0));
            Value::Null
        }
        "theme.current" => theme_snapshot(&crate::theme::theme()),
        "theme.byName" => {
            crate::theme::get_theme(&args.str(0)?).map(|theme| theme_snapshot(&theme)).unwrap_or(Value::Null)
        }
        "theme.available" => json!(crate::theme::get_available_themes()),
        "theme.systemName" => json!(crate::theme::SYSTEM_THEME_NAME),
        "theme.parseAuto" => json!(
            crate::theme::parse_auto_theme_setting(args.opt_str(0).as_deref())
                .map(|auto| json!({"lightTheme": auto.light_theme, "darkTheme": auto.dark_theme}))
        ),
        "images.convertToPng" => json!(
            crate::images::convert_to_png(&args.str(0)?, &args.str(1)?)
                .map(|(data, mime_type)| json!({"data": data, "mimeType": mime_type}))
        ),
        "theme.highlight" => json!(crate::theme::highlight_code(&args.str(0)?, args.opt_str(1).as_deref())),
        "theme.language" => json!(crate::theme::get_language_from_path(&args.str(0)?)),
        "config.info" => json!({
            "appName": APP_NAME,
            "configDirName": CONFIG_DIR_NAME,
            "version": env!("CARGO_PKG_VERSION"),
            "agentDir": runner.agent_dir().display().to_string(),
            "packageDir": std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.display().to_string())),
            "loadMode": runner.load_mode().map(|mode| json!({"mode": mode.as_str(), "hasUI": mode.has_ui()})),
        }),
        "messages.convertToLlm" => {
            let messages: Vec<pi_ai::Message> = serde_json::from_value(args.json(0)).map_err(error)?;
            json!(crate::messages::convert_to_llm(&messages))
        }
        "messages.serialize" => {
            let messages: Vec<pi_ai::Message> = serde_json::from_value(args.json(0)).map_err(error)?;
            json!(crate::compaction::serialize_conversation(&messages))
        }
        "messages.estimateTokens" => {
            let message: pi_ai::Message = serde_json::from_value(args.json(0)).map_err(error)?;
            json!(crate::compaction::estimate_tokens(&message))
        }
        "text.parseFrontmatter" => {
            let parsed = crate::resources::frontmatter::parse_frontmatter(&args.str(0)?)
                .map_err(|failure| error(failure.to_string()))?;
            json!({"frontmatter": parsed.frontmatter, "body": parsed.body})
        }
        "shell.config" => {
            let config = match args.str(0)?.as_str() {
                "powershell" => crate::platform::shell::powershell_config(),
                _ => crate::platform::shell::shell_config(None),
            };
            let config = config.map_err(error)?;
            json!({"shell": config.shell, "args": config.args})
        }
        "tools.builtinInfo" => builtin_info(runner, &args.str(0)?)?,
        "ai.catalog" => catalog(&args.str(0)?, args),
        "ai.credentialEnvVars" => json!(pi_ai::env::credential_env_vars()),
        "ai.hasApi" => {
            let api = args.str(0)?;
            json!(pi_ai::api_provider(&api).is_some() || runner.has_js_api(&api))
        }
        "ai.registerApi" => {
            runner.set_js_api(&args.str(0)?, true);
            Value::Null
        }
        "ai.unregisterApi" => {
            runner.set_js_api(&args.str(0)?, false);
            Value::Null
        }
        "provider.register" => {
            runner.register_provider(&args.str(0)?, args.json(1), &args.str(2)?).map_err(error)?;
            Value::Null
        }
        "provider.unregister" => {
            runner.unregister_provider(&args.str(0)?);
            Value::Null
        }
        "virtualModel.register" => {
            runner.register_virtual_model(args.json(0)).map_err(error)?;
            Value::Null
        }
        "virtualModel.unregister" => {
            runner.unregister_virtual_model(&args.str(0)?, &args.str(1)?);
            Value::Null
        }
        "mcp.register" => match runner.mcp.register(&args.str(0)?, &args.str(1)?, &args.json(2)) {
            Ok(()) => Value::Null,
            Err(message) => json!(message),
        },
        "mcp.check" => {
            let (path, name) = (args.str(0)?, args.str(1)?);
            match pi_mcp::config::validate_mcp_server_config(&name, &args.json(2)) {
                Err(failure) => json!(format!("Invalid MCP server registered by extension \"{path}\": {failure}")),
                Ok(_) => match runner.mcp.get(&name) {
                    Some(server) if server.extension_path != path => json!(format!(
                        "MCP server \"{name}\" is already registered by extension \"{}\"",
                        server.extension_path
                    )),
                    _ => Value::Null,
                },
            }
        }
        "mcp.unregister" => {
            runner.mcp.unregister(&args.str(0)?, &args.str(1)?);
            Value::Null
        }
        "mcp.list" => runner.mcp_servers_json(),
        "stream.event" => {
            let sender = lock(&runner.sinks).get(&args.u64(0)).cloned();
            if let (Some(sender), Some(event)) = (sender, super::convert::event_from_json(&args.json(1))) {
                sender.push(event);
            }
            Value::Null
        }
        "bash.data" => {
            let callback = lock(&runner.data).get(&args.u64(0)).cloned();
            if let Some(callback) = callback {
                callback(&args.str(1)?);
            }
            Value::Null
        }
        other => return Err(error(format!("Unknown host operation: {other}"))),
    };
    Ok(HostValue::Json(value))
}
