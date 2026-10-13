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
        pi_tui::TerminalColorMode::Truecolor => "truecolor",
        pi_tui::TerminalColorMode::Color256 => "256color",
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

fn catalog(kind: &str, args: &Args) -> Value {
    let models = pi_ai::builtin_models();
    match kind {
        "model" => {
            let (provider, id) = (args.opt_str(1), args.opt_str(2));
            json!(models.iter().find(|model| Some(&model.provider) == provider.as_ref() && Some(&model.id) == id.as_ref()))
        }
        "models" => {
            let provider = args.opt_str(1);
            let models: Vec<_> =
                models.iter().filter(|model| provider.as_ref().is_none_or(|wanted| &model.provider == wanted)).collect();
            json!(models)
        }
        _ => {
            let mut providers: Vec<&str> = models.iter().map(|model| model.provider.as_str()).collect();
            providers.dedup();
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
        "registry" => {
            runner.apply_registry(args.json(0));
            Value::Null
        }
        "theme.current" => theme_snapshot(&crate::theme::theme()),
        "theme.byName" => {
            crate::theme::get_theme(&args.str(0)?).map(|theme| theme_snapshot(&theme)).unwrap_or(Value::Null)
        }
        "theme.highlight" => json!(crate::theme::highlight_code(&args.str(0)?, args.opt_str(1).as_deref())),
        "theme.language" => json!(crate::theme::get_language_from_path(&args.str(0)?)),
        "config.info" => json!({
            "appName": APP_NAME,
            "configDirName": CONFIG_DIR_NAME,
            "version": env!("CARGO_PKG_VERSION"),
            "agentDir": runner.agent_dir().display().to_string(),
            "packageDir": std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.display().to_string())),
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
            let parsed = crate::resources::frontmatter::parse_frontmatter(&args.str(0)?).map_err(|failure| error(failure.to_string()))?;
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
        "ai.envApiKey" => json!(pi_ai::env::get_env_api_key(&args.str(0)?)),
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
            if let (Some(sender), Ok(event)) = (sender, serde_json::from_value(args.json(1))) {
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
