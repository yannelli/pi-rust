//! Port of upstream `cli/experimental/command.ts`: the option scanner shared by the experimental commands.

use std::collections::HashMap;

/// Checks one option value; the scanner keeps the raw text of values that pass.
pub type OptionCheck = fn(&str) -> Result<(), String>;

/// Upstream `CommandOption`: a flag, or an option that takes a value.
#[derive(Clone, Copy, Debug)]
pub struct CommandOption {
    pub name: &'static str,
    pub flag: bool,
    pub repeatable: bool,
    pub check: Option<OptionCheck>,
}

/// Upstream `stringOption`.
pub const fn string_option(name: &'static str, repeatable: bool) -> CommandOption {
    CommandOption { name, flag: false, repeatable, check: None }
}

/// Upstream `valueOption`: a value option whose text must pass `check`.
pub const fn value_option(name: &'static str, check: OptionCheck) -> CommandOption {
    CommandOption { name, flag: false, repeatable: false, check: Some(check) }
}

/// Upstream `flagOption`.
pub const fn flag_option(name: &'static str) -> CommandOption {
    CommandOption { name, flag: true, repeatable: false, check: None }
}

/// Upstream `ParsedCommandInput`: option values, and the arguments from the first unknown one on.
#[derive(Debug, Default)]
pub struct ParsedCommandInput {
    pub remaining_args: Vec<String>,
    values: HashMap<&'static str, Vec<String>>,
}

impl ParsedCommandInput {
    pub fn value(&self, option: &CommandOption) -> Option<String> {
        self.values.get(option.name).and_then(|values| values.first()).cloned()
    }

    pub fn values(&self, option: &CommandOption) -> Vec<String> {
        self.values.get(option.name).cloned().unwrap_or_default()
    }

    pub fn flag(&self, option: &CommandOption) -> bool {
        self.values.contains_key(option.name)
    }
}

/// Upstream `Command.parseOptions`: scan known options until `--` or the first unknown argument.
pub fn parse_options(options: &[CommandOption], argv: &[String]) -> (ParsedCommandInput, Vec<String>) {
    let mut parsed = ParsedCommandInput::default();
    let mut errors = Vec::new();
    let mut index = 0;
    while index < argv.len() {
        let argument = &argv[index];
        index += 1;
        if argument == "--" {
            parsed.remaining_args.extend_from_slice(&argv[index - 1..]);
            break;
        }
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (argument.as_str(), None),
        };
        let Some(option) = options.iter().find(|option| option.name == name) else {
            parsed.remaining_args.extend_from_slice(&argv[index - 1..]);
            break;
        };
        let value = if option.flag {
            if inline.is_some() {
                errors.push(format!("{name} does not take a value"));
                continue;
            }
            String::new()
        } else {
            let mut candidate = inline.map(str::to_string);
            if candidate.is_none()
                && let Some(next) = argv.get(index).filter(|next| !next.starts_with('-'))
            {
                candidate = Some(next.clone());
                index += 1;
            }
            match candidate {
                Some(candidate) if !candidate.is_empty() => candidate,
                _ => {
                    errors.push(format!("{name} requires a value"));
                    continue;
                }
            }
        };
        let values = parsed.values.entry(option.name).or_default();
        if !values.is_empty() && !option.repeatable {
            errors.push(format!("{name} may only be specified once"));
            continue;
        }
        if let Some(Err(error)) = option.check.map(|check| check(&value)) {
            errors.push(error);
            continue;
        }
        values.push(value);
    }
    parsed.values.retain(|_, values| !values.is_empty());
    (parsed, errors)
}
