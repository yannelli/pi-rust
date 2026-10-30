//! Argument parsing and help text of `pi install`, `pi remove`, `pi update`, and `pi list`
//! (upstream `parsePackageCommand` and `printPackageCommandHelp`).

use crate::resources::brand::{APP_NAME, CONFIG_DIR_NAME};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Install,
    Remove,
    Update,
    List,
}

impl Command {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Remove => "remove",
            Self::Update => "update",
            Self::List => "list",
        }
    }

    pub(crate) fn usage(self) -> String {
        match self {
            Self::Install => format!("{APP_NAME} install <source> [-l] [--approve|--no-approve]"),
            Self::Remove => format!("{APP_NAME} remove <source> [-l] [--approve|--no-approve]"),
            Self::Update => format!(
                "{APP_NAME} update [source|self|pi] [--self|--extensions|--models|--all] [--extension <source>] [--approve|--no-approve] [--force]"
            ),
            Self::List => format!("{APP_NAME} list [--approve|--no-approve]"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UpdateTarget {
    All,
    SelfOnly,
    Extensions(Option<String>),
    Models,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub command: Command,
    pub source: Option<String>,
    pub update_target: Option<UpdateTarget>,
    pub show_extensions_skipped_note: bool,
    pub local: bool,
    pub force: bool,
    pub trust_override: Option<bool>,
    pub help: bool,
    pub problem: Option<Problem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Problem {
    InvalidOption(String),
    MissingValue(String),
    InvalidArgument(String),
    Conflict(String),
}

fn first(slot: &mut Option<Problem>, problem: Problem) {
    if slot.is_none() {
        *slot = Some(problem);
    }
}

/// Parse `args` (starting with the subcommand); `None` when it is not a package command.
pub(crate) fn parse_package_command(args: &[String]) -> Option<Parsed> {
    let (raw, rest) = args.split_first()?;
    let command = match raw.as_str() {
        "install" => Command::Install,
        "remove" | "uninstall" => Command::Remove,
        "update" => Command::Update,
        "list" => Command::List,
        _ => return None,
    };
    let is_update = command == Command::Update;
    let mut parsed = Parsed {
        command,
        source: None,
        update_target: None,
        show_extensions_skipped_note: false,
        local: false,
        force: false,
        trust_override: None,
        help: false,
        problem: None,
    };
    let (mut invalid_option, mut missing, mut invalid_argument, mut conflict) = (None, None, None, None);
    let (mut self_flag, mut extensions_flag, mut models_flag, mut all_flag) = (false, false, false, false);
    let mut extension_source: Option<String> = None;
    let mut index = 0;
    while index < rest.len() {
        let arg = rest[index].as_str();
        let invalid = |slot: &mut Option<Problem>| first(slot, Problem::InvalidOption(arg.to_string()));
        match arg {
            "-h" | "--help" => parsed.help = true,
            "-l" | "--local" if matches!(command, Command::Install | Command::Remove) => parsed.local = true,
            "--self" if is_update => self_flag = true,
            "--extensions" if is_update => extensions_flag = true,
            "--models" if is_update => models_flag = true,
            "--all" if is_update => all_flag = true,
            "--force" if is_update => parsed.force = true,
            "--approve" | "-a" => parsed.trust_override = Some(true),
            "--no-approve" | "-na" => parsed.trust_override = Some(false),
            "--extension" if is_update => match rest.get(index + 1).filter(|value| !value.starts_with('-')) {
                None => first(&mut missing, Problem::MissingValue(arg.to_string())),
                Some(value) => {
                    index += 1;
                    if extension_source.is_some() {
                        first(&mut conflict, Problem::Conflict("--extension can only be provided once".into()));
                    } else {
                        extension_source = Some(value.clone());
                    }
                }
            },
            _ if arg.starts_with('-') => invalid(&mut invalid_option),
            _ if parsed.source.is_none() => parsed.source = Some(arg.to_string()),
            _ => first(&mut invalid_argument, Problem::InvalidArgument(arg.to_string())),
        }
        index += 1;
    }
    if is_update {
        let source = parsed.source.clone();
        let mut conflicts = |message: &str| first(&mut conflict, Problem::Conflict(message.into()));
        if all_flag && (self_flag || extensions_flag || models_flag || extension_source.is_some()) {
            conflicts("--all cannot be combined with --self, --extensions, --models, or --extension");
        }
        if all_flag && source.is_some() {
            conflicts("--all cannot be combined with a positional source");
        }
        let target = if models_flag {
            if self_flag || extensions_flag || all_flag || extension_source.is_some() {
                conflicts("--models cannot be combined with --self, --extensions, --all, or --extension");
            }
            if source.is_some() {
                conflicts("--models cannot be combined with a positional source");
            }
            UpdateTarget::Models
        } else if let Some(extension) = extension_source {
            if self_flag || extensions_flag || all_flag {
                conflicts("--extension cannot be combined with --self, --extensions, or --all");
            }
            if source.is_some() {
                conflicts("--extension cannot be combined with a positional source");
            }
            UpdateTarget::Extensions(Some(extension))
        } else if let Some(source) = source {
            if source == "self" || source == "pi" {
                if extensions_flag { UpdateTarget::All } else { UpdateTarget::SelfOnly }
            } else {
                if extensions_flag || self_flag || all_flag {
                    conflicts("positional update targets cannot be combined with --self, --extensions, or --all");
                }
                UpdateTarget::Extensions(Some(source))
            }
        } else if all_flag || (self_flag && extensions_flag) {
            UpdateTarget::All
        } else if self_flag {
            UpdateTarget::SelfOnly
        } else if extensions_flag {
            UpdateTarget::Extensions(None)
        } else {
            parsed.show_extensions_skipped_note = true;
            UpdateTarget::SelfOnly
        };
        parsed.update_target = Some(target);
    }
    parsed.problem = invalid_option.or(missing).or(invalid_argument).or(conflict);
    Some(parsed)
}

pub(crate) fn help_text(command: Command) -> String {
    let usage = command.usage();
    let trust = "  -a, --approve     Trust project-local files for this command\n  -na, --no-approve Ignore project-local files for this command";
    match command {
        Command::Install => format!(
            "Usage:\n  {usage}\n\nInstall a package and add it to settings.\n\nOptions:\n  -l, --local       Install project-locally ({CONFIG_DIR_NAME}/settings.json)\n{trust}\n\nExamples:\n  {APP_NAME} install marketplace:todo\n  {APP_NAME} install npm:@foo/bar\n  {APP_NAME} install git:github.com/user/repo\n  {APP_NAME} install git:git@github.com:user/repo\n  {APP_NAME} install https://github.com/user/repo\n  {APP_NAME} install ssh://git@github.com/user/repo\n  {APP_NAME} install https://example.com/pkg.tar.gz\n  {APP_NAME} install ./local/path\n"
        ),
        Command::Remove => format!(
            "Usage:\n  {usage}\n\nRemove a package and its source from settings.\nAlias: {APP_NAME} uninstall <source> [-l]\n\nOptions:\n  -l, --local       Remove from project settings ({CONFIG_DIR_NAME}/settings.json)\n{trust}\n\nExamples:\n  {APP_NAME} remove npm:@foo/bar\n  {APP_NAME} uninstall npm:@foo/bar\n"
        ),
        Command::Update => format!(
            "Usage:\n  {usage}\n\nUpdate pi, installed packages, or model catalogs.\n\nOptions:\n  --self                  Update pi only (default when no target is given)\n  --extensions            Update installed packages only\n  --models                Refresh model catalogs only\n  --all                   Update pi and installed packages\n  --extension <source>    Update one package only\n  -a, --approve           Trust project-local files for this command\n  -na, --no-approve       Ignore project-local files for this command\n  --force                 Reinstall pi even if the current version is latest\n\nShort forms:\n  {APP_NAME} update                Update pi only\n  {APP_NAME} update --all          Update pi and all extensions\n  {APP_NAME} update --models       Refresh model catalogs only\n  {APP_NAME} update <source>       Update one package\n  {APP_NAME} update pi             Update pi only (self works as alias to pi)\n"
        ),
        Command::List => format!(
            "Usage:\n  {usage}\n\nList installed packages from user and project settings.\n\nOptions:\n  -a, --approve      Trust project-local files for this command\n  -na, --no-approve  Ignore project-local files for this command\n"
        ),
    }
}
