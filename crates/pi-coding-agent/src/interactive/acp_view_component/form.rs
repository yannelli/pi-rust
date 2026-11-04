//! Add and edit forms, using the browser's text input component.

use std::collections::BTreeMap;

use pi_tui::Component;
use pi_tui::components::{Input, InputOptions};
use pi_tui::keys::matches_key;

use super::{AcpAction, AcpRow, words};
use crate::acp_client::{AcpAgentEntry, AcpScope, provider_id};
use crate::theme::{ThemeColor, theme};

const NAME: usize = 0;
const ID: usize = 1;
const COMMAND: usize = 2;
const ARGS: usize = 3;
const LOGIN: usize = 4;
const DESCRIPTION: usize = 5;
const ENV: usize = 6;
const SCOPE: usize = 7;
const ENABLED: usize = 8;
const SAVE: usize = 9;
const LABELS: [&str; 6] = ["Name", "ID", "Command", "Arguments", "Login command", "Description"];

pub(super) struct AgentForm {
    inputs: Vec<Input>,
    environment: Vec<Input>,
    env_line: usize,
    env_paste: Option<String>,
    field: usize,
    original: Option<AcpAgentEntry>,
    scope: AcpScope,
    enabled: bool,
    id_edited: bool,
    pub error: Option<String>,
}

const ENV_PROMPT: &str = "   ";

fn input(value: &str) -> Input {
    let mut input = Input::new(InputOptions { prompt: Some(ENV_PROMPT.into()), ..InputOptions::default() });
    input.set_value(value);
    input
}

fn field_prompt(index: usize, editing: bool) -> String {
    let suffix = if index == ID && editing { " (read-only)" } else { "" };
    format!(" {}{suffix}: ", LABELS[index])
}

/// The focused input with its cursor; the others as plain text, so one cursor shows.
fn render_input(input: &mut Input, prompt: &str, focused: bool, width: usize) -> Vec<String> {
    input.focused = focused;
    if focused {
        return input.render(width);
    }
    vec![pi_tui::utils::truncate_to_width_with(&format!("{prompt}{}", input.get_value()), width, "…", false)]
}

fn slug(name: &str) -> String {
    name.to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_lowercase() && !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

impl AgentForm {
    pub fn new(entry: Option<AcpAgentEntry>, scope: AcpScope) -> Self {
        let values = entry.as_ref().map_or_else(
            || vec![String::new(); LABELS.len()],
            |entry| {
                vec![
                    entry.name.clone(),
                    entry.id.clone(),
                    entry.command.clone(),
                    words::join(&entry.args),
                    words::join(entry.login_command.as_deref().unwrap_or_default()),
                    entry.description.clone().unwrap_or_default(),
                ]
            },
        );
        let mut environment: Vec<Input> = entry
            .as_ref()
            .map(|entry| &entry.env)
            .into_iter()
            .flatten()
            .map(|(key, value)| input(&format!("{key}={value}")))
            .collect();
        environment.push(input(""));
        Self {
            inputs: values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let mut field = Input::new(InputOptions {
                        prompt: Some(field_prompt(index, entry.is_some())),
                        ..InputOptions::default()
                    });
                    field.set_value(value);
                    field
                })
                .collect(),
            environment,
            env_line: 0,
            env_paste: None,
            field: NAME,
            enabled: entry.as_ref().is_none_or(|entry| entry.enabled),
            id_edited: entry.is_some(),
            original: entry,
            scope,
            error: None,
        }
    }

    pub fn target(&self) -> (&str, AcpScope) {
        (self.inputs[ID].get_value(), self.scope)
    }

    pub fn editing(&self) -> bool {
        self.original.is_some()
    }

    fn move_field(&mut self, backwards: bool) {
        loop {
            self.field = if backwards { (self.field + SAVE) % (SAVE + 1) } else { (self.field + 1) % (SAVE + 1) };
            if !self.editing() || !matches!(self.field, ID | SCOPE) {
                break;
            }
        }
    }

    /// Returns true when the user requests Save.
    pub fn handle(&mut self, data: &str) -> bool {
        if self.env_paste.is_some() {
            self.handle_env(data);
            return false;
        }
        if matches_key(data, "ctrl+s") || (self.field == SAVE && matches_key(data, "enter")) {
            return true;
        }
        if matches_key(data, "tab") || matches_key(data, "shift+tab") {
            self.move_field(matches_key(data, "shift+tab"));
        } else if matches_key(data, "up") || matches_key(data, "down") {
            let up = matches_key(data, "up");
            if self.field == ENV && up && self.env_line > 0 {
                self.env_line -= 1;
            } else if self.field == ENV && !up && self.env_line + 1 < self.environment.len() {
                self.env_line += 1;
            } else {
                self.move_field(up);
            }
        } else if self.field == SCOPE {
            if matches_key(data, "enter") || data == " " || matches_key(data, "left") || matches_key(data, "right") {
                self.scope = match self.scope {
                    AcpScope::Global => AcpScope::Project,
                    AcpScope::Project => AcpScope::Global,
                };
            }
        } else if self.field == ENABLED {
            if matches_key(data, "enter") || data == " " || matches_key(data, "left") || matches_key(data, "right") {
                self.enabled = !self.enabled;
            }
        } else if self.field == ENV {
            self.handle_env(data);
        } else if self.field < self.inputs.len() {
            if matches_key(data, "enter") {
                self.move_field(false);
            } else {
                self.inputs[self.field].handle_input(data);
                if self.field == ID {
                    self.id_edited = true;
                } else if self.field == NAME && !self.id_edited {
                    let id = slug(self.inputs[NAME].get_value());
                    self.inputs[ID].set_value(&id);
                }
            }
        }
        false
    }

    fn handle_env(&mut self, data: &str) {
        if let Some(text) = data.strip_prefix("\x1b[200~") {
            self.env_paste = Some(text.into());
        } else if let Some(paste) = &mut self.env_paste {
            paste.push_str(data);
        }
        if let Some(paste) = &self.env_paste {
            if let Some(end) = paste.find("\x1b[201~") {
                let text = paste[..end].to_string();
                self.env_paste = None;
                self.paste_env(&text);
            }
            return;
        }
        if matches_key(data, "enter") {
            self.env_line += 1;
            self.environment.insert(self.env_line, input(""));
        } else if data.contains('\n') || data.contains('\r') {
            self.paste_env(data);
        } else {
            self.environment[self.env_line].handle_input(data);
        }
    }

    fn paste_env(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                self.env_line += 1;
                self.environment.insert(self.env_line, input(""));
            }
            self.environment[self.env_line].handle_input(&format!("\x1b[200~{line}\x1b[201~"));
        }
    }

    pub fn action(&self, installed: &[AcpRow]) -> Result<AcpAction, String> {
        let name = self.inputs[NAME].get_value().trim();
        if name.is_empty() {
            return Err("Name is required.".into());
        }
        let id = self.inputs[ID].get_value();
        if id.is_empty() || !id.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') {
            return Err("ID must use lowercase letters, digits, and dashes.".into());
        }
        if !self.editing() && installed.iter().any(|row| row.id == id) {
            return Err(format!("ID \"{id}\" is already installed. Choose another ID."));
        }
        let provider = provider_id(id);
        if let Some(row) = installed.iter().find(|row| row.id != id && provider_id(&row.id) == provider) {
            return Err(format!("Provider \"{provider}\" already belongs to {}. Choose another ID.", row.name));
        }
        let command = self.inputs[COMMAND].get_value().trim();
        if command.is_empty() {
            return Err("Command is required.".into());
        }
        let args = words::split(self.inputs[ARGS].get_value()).map_err(|error| format!("Arguments: {error}."))?;
        let login = words::split(self.inputs[LOGIN].get_value()).map_err(|error| format!("Login command: {error}."))?;
        if login.first().is_some_and(|command| command.is_empty()) {
            return Err("Login command must start with an executable name or path.".into());
        }
        let mut env = BTreeMap::new();
        for (index, line) in self.environment.iter().enumerate() {
            let line = line.get_value();
            if line.trim().is_empty() {
                continue;
            }
            let invalid = || format!("Environment line {} must be KEY=VALUE with a valid variable name.", index + 1);
            let (key, value) = line.split_once('=').ok_or_else(invalid)?;
            if key.is_empty()
                || !key
                    .chars()
                    .enumerate()
                    .all(|(i, ch)| ch == '_' || ch.is_ascii_alphabetic() || (i > 0 && ch.is_ascii_digit()))
            {
                return Err(invalid());
            }
            env.insert(key.into(), value.into());
        }
        let description = self.inputs[DESCRIPTION].get_value().trim();
        let original = self.original.as_ref();
        let entry = AcpAgentEntry {
            id: id.into(),
            name: name.into(),
            command: command.into(),
            args,
            env,
            enabled: self.enabled,
            description: (!description.is_empty()).then(|| description.into()),
            login_command: (!login.is_empty()).then_some(login),
            source: original.map_or("custom", |entry| entry.source.as_str()).into(),
            version: original.and_then(|entry| entry.version.clone()),
            install_dir: original.and_then(|entry| entry.install_dir.clone()),
        };
        Ok(AcpAction::Save { entry: Box::new(entry), scope: self.scope, editing: self.editing() })
    }

    pub fn render(&mut self, width: usize) -> Vec<String> {
        let t = theme();
        let title = if self.editing() { "Edit agent" } else { "Add custom agent" };
        let mut lines = vec![format!(" {}", t.fg(ThemeColor::Accent, &t.bold(title)))];
        let editing = self.editing();
        for (index, field) in self.inputs.iter_mut().enumerate() {
            let focused = self.field == index;
            let color = if focused { ThemeColor::Accent } else { ThemeColor::Muted };
            let rendered = render_input(field, &field_prompt(index, editing), focused, width);
            lines.extend(rendered.into_iter().map(|line| t.fg(color, &line)));
        }
        lines.push(t.fg(ThemeColor::Dim, &format!(" Provider: {}", provider_id(self.inputs[ID].get_value()))));
        lines.push(t.fg(
            if self.field == ENV { ThemeColor::Accent } else { ThemeColor::Muted },
            " Environment (KEY=VALUE, one per line; Enter adds a line)",
        ));
        let start = self.env_line.saturating_sub(2);
        for (index, line) in self.environment.iter_mut().enumerate().skip(start).take(4) {
            let focused = self.field == ENV && index == self.env_line;
            lines.extend(render_input(line, ENV_PROMPT, focused, width));
        }
        for (field, label) in [
            (SCOPE, format!("Scope: {}{}", self.scope.label(), if self.editing() { " (read-only)" } else { "" })),
            (ENABLED, format!("Enabled: {}", if self.enabled { "on" } else { "off" })),
            (SAVE, "Save".into()),
        ] {
            let prefix = if self.field == field { "→" } else { " " };
            lines.push(t.fg(
                if self.field == field { ThemeColor::Accent } else { ThemeColor::Muted },
                &format!(" {prefix} {label}"),
            ));
        }
        if let Some(error) = &self.error {
            lines.extend(pi_tui::utils::wrap_text_with_ansi(&t.fg(ThemeColor::Error, &format!(" {error}")), width));
        }
        lines
    }
}
