//! `/acp` actions that need the session or the terminal: log in, set as default, and `/acp mode`.

use std::rc::Rc;

use pi_ai::providers::acp::{self, AuthMethod, DEFAULT_MODEL, TerminalLogin};
use pi_tui::SlashCommand;
use pi_tui::autocomplete::CommandEntry;

use super::mode::InteractiveMode;
use crate::acp_client::registry::RegistryAgent;
use crate::acp_client::{AcpAgentEntry, AcpScope, host, provider_id, register, store};
use crate::extension_ui::{DialogOptions, ExtensionUi};

/// `/acp:<name>` runs an agent command whose name a pi command already uses.
const COMMAND_PREFIX: &str = "acp:";

impl InteractiveMode {
    /// The slash commands of the ACP agent of the current model, for the editor. A name that
    /// `taken` holds gets the `acp:` prefix, which the provider removes before sending.
    pub(super) fn acp_slash_commands(&self, taken: &[CommandEntry]) -> Vec<CommandEntry> {
        let Some(model) = self.session().model().filter(|model| model.api == acp::API) else { return Vec::new() };
        let commands = host::commands(&self.agent_dir, &model.provider);
        commands
            .into_iter()
            .map(|command| {
                let free = !taken.iter().any(|entry| entry.name() == command.name);
                let name = if free { command.name.clone() } else { format!("{COMMAND_PREFIX}{}", command.name) };
                let mut entry = SlashCommand::new(name, Some(format!("[acp] {}", command.description).trim_end()));
                if let Some(hint) = command.hint {
                    entry = entry.with_argument_hint(hint);
                }
                entry.into()
            })
            .collect()
    }

    /// Rebuilds the editor commands after a turn of an ACP agent, which can change its commands.
    pub(super) fn refresh_acp_commands(&self) {
        if self.session().model().is_some_and(|model| model.api == acp::API) {
            self.setup_autocomplete_provider();
        }
    }

    /// Shows the auth methods of the agent and runs the chosen one: a terminal method runs while
    /// the TUI is stopped, an agent method goes to `authenticate`.
    pub(super) fn acp_login(&self, entry: AcpAgentEntry) {
        let launch = register::launch(&entry);
        let cwd = self.session().cwd().to_path_buf();
        self.spawn(move |mode| async move {
            mode.show_status(&format!("Starting {}...", entry.name));
            let (probe, dir) = (launch.clone(), cwd.clone());
            let methods = tokio::spawn(async move { acp::auth_methods(&probe, &dir).await }).await;
            let methods: Vec<AuthMethod> = match methods {
                Ok(Ok(methods)) => methods,
                Ok(Err(error)) => match &entry.login_command {
                    Some(command) => vec![fallback_method(command)],
                    None => return mode.show_error(&error),
                },
                Err(error) => return mode.show_error(&error.to_string()),
            };
            if methods.is_empty() {
                return mode.show_status(&format!("{} has no login.", entry.name));
            }
            let labels: Vec<String> = methods.iter().map(label).collect();
            let title = format!("Log in to {}", entry.name);
            let Some(chosen) = mode.extension_ui().select(&title, &labels, DialogOptions::default()).await else {
                return mode.show_status("Login cancelled");
            };
            let Some(method) = labels.iter().position(|label| *label == chosen).map(|index| methods[index].clone())
            else {
                return;
            };
            if let Some(terminal) = &method.terminal {
                if let Err(error) = run_in_terminal(&mode, terminal).await {
                    return mode.show_error(&error);
                }
            }
            mode.show_status(&format!("Checking the {} login...", entry.name));
            let method_id = method.terminal.is_none().then(|| method.id.clone());
            let result =
                tokio::spawn(async move { acp::authenticate(&launch, &cwd, method_id.as_deref()).await }).await;
            match result {
                Ok(Ok(())) => mode.show_status(&format!("Logged in to {}", entry.name)),
                Ok(Err(error)) => mode.show_error(&error),
                Err(error) => mode.show_error(&error.to_string()),
            }
        });
    }

    /// Offers the official installer of an agent program that is not installed, runs it in the
    /// terminal, then adds the agent to the `scope` settings.
    pub(super) fn acp_install_program(&self, agent: RegistryAgent, scope: AcpScope) {
        let agent_dir = self.agent_dir.clone();
        self.spawn(move |mode| async move {
            let Some(local) = agent.local.clone() else { return };
            let shown = local.installer.last().cloned().unwrap_or_default();
            let title = format!("{} is not installed. Run its installer in the terminal?\n{shown}", local.program);
            let options = ["Run the installer".to_string(), "Cancel".to_string()];
            let choice = mode.extension_ui().select(&title, &options, DialogOptions::default()).await;
            if choice.as_deref() != Some(options[0].as_str()) {
                return mode.show_status("Install cancelled");
            }
            let installer = TerminalLogin {
                command: local.installer[0].clone(),
                args: local.installer[1..].to_vec(),
                env: Default::default(),
            };
            if let Err(error) = run_in_terminal(&mode, &installer).await {
                return mode.show_error(&error);
            }
            let (name, dir) = (agent.name.clone(), agent_dir.clone());
            let entry = tokio::spawn(async move { crate::acp_client::install::install(&agent, &dir).await }).await;
            let entry = match entry {
                Ok(Ok(entry)) => entry,
                Ok(Err(error)) => return mode.show_error(&error),
                Err(error) => return mode.show_error(&error.to_string()),
            };
            if let Err(error) = store::upsert(&mode.settings(), &entry, scope) {
                return mode.show_error(&error);
            }
            crate::acp_client::providers::sync_providers(&mode.session()).await;
            mode.show_status(&format!("Installed {name} in the {} settings. Run /acp to log in.", scope.label()));
        });
    }

    /// Selects `<provider>/default` for this session and saves it as the default model.
    pub(super) fn acp_set_default(&self, entry: AcpAgentEntry) {
        let session = self.session();
        let provider = provider_id(&entry.id);
        self.spawn(move |mode| async move {
            let Some(model) = session.model_runtime().get_model(&provider, DEFAULT_MODEL) else {
                return mode.show_error(&format!("{} is not enabled", entry.name));
            };
            match session.set_model(model, true).await {
                Ok(()) => mode.show_status(&format!("{} is the default model provider", entry.name)),
                Err(error) => mode.show_error(&format!("{error:#}")),
            }
        });
    }

    /// `/acp mode`: the modes, permissions, and other settings the ACP agent of the current model
    /// advertises; its session opens first when none runs.
    pub(super) fn acp_mode(&self) {
        let session = self.session();
        let Some(model) = session.model().filter(|model| model.api == acp::API) else {
            return self.show_warning("The current model is not an ACP agent. Select one with /model.");
        };
        if !self.current_model_is_acp() {
            return self.show_warning(&format!("No ACP agent is set up for {}. Run /acp.", model.provider));
        }
        self.spawn(move |mode| async move {
            let Some(info) = mode.prepare_acp_session().await else { return };
            let selectors: Vec<_> =
                info.selectors.into_iter().filter(|selector| selector.category != "model").collect();
            if selectors.is_empty() {
                return mode.show_status("The agent offers no modes.");
            }
            let ui = mode.extension_ui();
            let selector = match selectors.len() {
                1 => selectors[0].clone(),
                _ => {
                    let names: Vec<String> = selectors.iter().map(|s| s.name.clone()).collect();
                    let Some(name) = ui.select("Agent setting", &names, DialogOptions::default()).await else { return };
                    let Some(selector) = selectors.iter().find(|s| s.name == name) else { return };
                    selector.clone()
                }
            };
            let labels: Vec<String> = selector
                .choices
                .iter()
                .map(|choice| match selector.current.as_deref() == Some(choice.id.as_str()) {
                    true => format!("{} (current)", choice.name),
                    false => choice.name.clone(),
                })
                .collect();
            let Some(chosen) = ui.select(&selector.name, &labels, DialogOptions::default()).await else { return };
            let Some(index) = labels.iter().position(|label| *label == chosen) else { return };
            let value = selector.choices[index].id.clone();
            let (provider, id, pi_session) = (model.provider.clone(), selector.id.clone(), session.session_id());
            let result =
                tokio::spawn(async move { acp::set_session_option(&provider, &pi_session, &id, &value).await }).await;
            match result {
                Ok(Ok(())) => {
                    mode.show_status(&format!("{}: {}", selector.name, selector.choices[index].name));
                    if let Some(info) = acp::session_info(&model.provider, &session.session_id()) {
                        mode.apply_acp_session_info(&info);
                    }
                }
                Ok(Err(error)) => mode.show_error(&error),
                Err(error) => mode.show_error(&error.to_string()),
            }
        });
    }
}

fn fallback_method(command: &[String]) -> AuthMethod {
    let terminal = TerminalLogin {
        command: command.first().cloned().unwrap_or_default(),
        args: command.iter().skip(1).cloned().collect(),
        env: Default::default(),
    };
    AuthMethod { id: "login".into(), name: command.join(" "), description: None, terminal: Some(terminal) }
}

fn label(method: &AuthMethod) -> String {
    match (&method.description, &method.terminal) {
        (Some(description), _) => format!("{}: {description}", method.name),
        (None, Some(terminal)) => format!("{} ({} {})", method.name, terminal.command, terminal.args.join(" ")),
        (None, None) => method.name.clone(),
    }
}

/// Runs a terminal login with the terminal handed over from the TUI.
async fn run_in_terminal(mode: &Rc<InteractiveMode>, login: &TerminalLogin) -> Result<(), String> {
    let tui = mode.tui();
    tui.stop(pi_tui::TuiStopOptions::default());
    println!("\nRunning {} {}\n", login.command, login.args.join(" "));
    let status = tokio::process::Command::new(&login.command).args(&login.args).envs(&login.env).status().await;
    tui.start();
    mode.request_full_render();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("{} exited with {status}", login.command)),
        Err(error) => Err(format!("Cannot run {}: {error}", login.command)),
    }
}
