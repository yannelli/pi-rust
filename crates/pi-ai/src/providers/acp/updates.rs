//! `session/update` kinds that change the session rather than the reply: config options, the
//! mode, the slash commands, and the context window size.

use serde_json::Value;

use super::host::AgentCommand;
use super::session::Runtime;

/// The commands of an `available_commands_update`.
pub(crate) fn commands(update: &Value) -> Vec<AgentCommand> {
    let commands = update["availableCommands"].as_array().into_iter().flatten();
    commands
        .filter_map(|command| {
            let name = command["name"].as_str()?.trim_start_matches('/');
            Some(AgentCommand {
                name: name.to_string(),
                description: command["description"].as_str().unwrap_or_default().to_string(),
                hint: command.pointer("/input/hint").and_then(Value::as_str).map(str::to_string),
            })
        })
        .filter(|command| !command.name.is_empty())
        .collect()
}

/// Applies a session update. Returns whether it was one, and the host should hear of the change.
pub(crate) fn apply(runtime: &mut Runtime, update: &Value) -> bool {
    match update["sessionUpdate"].as_str().unwrap_or_default() {
        "config_option_update" => {
            runtime.apply_config(update["configOptions"].as_array().map_or(&[][..], Vec::as_slice))
        }
        "current_mode_update" => {
            let mode = runtime.selectors.iter().find(|selector| selector.category == "mode").map(|s| s.id.clone());
            runtime.set_current(&mode.unwrap_or_default(), update["currentModeId"].as_str().unwrap_or_default());
        }
        "available_commands_update" => runtime.commands = commands(update),
        "usage_update" => match update["size"].as_u64().filter(|size| *size > 0) {
            Some(size) => runtime.context_window = Some(size),
            None => return false,
        },
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn commands_keep_name_description_and_hint() {
        let update = json!({"availableCommands": [
            {"name": "web", "description": "Search the web", "input": {"hint": "query"}},
            {"name": "/plan", "description": "Plan"}, {"description": "no name"}]});
        let parsed = commands(&update);
        assert_eq!(parsed.len(), 2);
        assert_eq!((parsed[0].name.as_str(), parsed[0].hint.as_deref()), ("web", Some("query")));
        assert_eq!(parsed[1].name, "plan");
    }
}
