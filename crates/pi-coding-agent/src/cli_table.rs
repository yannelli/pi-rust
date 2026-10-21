//! The options and subcommands of `pi`. `--help` and `pi completion` both render this table,
//! so the help text and the completion scripts list the same flags.

/// What the value of an option completes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    /// A switch, or an optional value with nothing to complete.
    None,
    /// Free text.
    Text,
    File,
    Dir,
    /// `provider/id` from the model catalog and `models.json`.
    Model,
    Provider,
    Choice(&'static [&'static str]),
}

/// One option of `pi --help`.
#[derive(Clone, Copy, Debug)]
pub struct CliOption {
    pub long: &'static str,
    pub short: Option<&'static str>,
    /// Value placeholder shown in `--help`.
    pub value: Option<&'static str>,
    pub kind: ValueKind,
    /// Description; later entries are continuation lines.
    pub help: &'static [&'static str],
}

impl CliOption {
    /// `--` ends option parsing; it is listed in `--help` but never completed.
    pub fn is_separator(&self) -> bool {
        self.long == "--"
    }

    /// Whether the option consumes the next argument.
    pub fn takes_value(&self) -> bool {
        self.kind != ValueKind::None
    }
}

/// A subcommand dispatched before the arguments become a prompt.
#[derive(Clone, Copy, Debug)]
pub struct CliCommand {
    pub name: &'static str,
    /// Arguments shown after the name in `--help`.
    pub usage: &'static str,
    pub help: &'static str,
    /// Subcommands and flags completed after the name.
    pub words: &'static [&'static str],
}

const fn flag(long: &'static str, short: Option<&'static str>, help: &'static [&'static str]) -> CliOption {
    CliOption { long, short, value: None, kind: ValueKind::None, help }
}

const fn valued(
    long: &'static str,
    short: Option<&'static str>,
    value: &'static str,
    kind: ValueKind,
    help: &'static [&'static str],
) -> CliOption {
    CliOption { long, short, value: Some(value), kind, help }
}

const fn command(
    name: &'static str,
    usage: &'static str,
    help: &'static str,
    words: &'static [&'static str],
) -> CliCommand {
    CliCommand { name, usage, help, words }
}

use ValueKind::{Choice, Dir, File, Model, Provider, Text};

pub const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];
pub const COMPLETION_SHELLS: &[&str] = &["bash", "zsh", "fish", "powershell"];
const TRUST: &[&str] = &["-a", "--approve", "-na", "--no-approve"];
const PACKAGE: &[&str] = &["-l", "--local", "-a", "--approve", "-na", "--no-approve"];

/// The options of `pi --help`, in order.
pub const CLI_OPTIONS: &[CliOption] = &[
    valued("--provider", None, "<name>", Provider, &["Provider to search for --model (requires --model)"]),
    valued(
        "--model",
        None,
        "<pattern>",
        Model,
        &["Model pattern or ID (supports \"provider/id\" and optional \":<thinking>\")"],
    ),
    valued("--api-key", None, "<key>", Text, &["API key (defaults to env vars)"]),
    valued("--system-prompt", None, "<text>", File, &["System prompt text or file (default: coding assistant prompt)"]),
    valued(
        "--append-system-prompt",
        None,
        "<text>",
        File,
        &["Append text or file contents to the system prompt (can be used multiple times)"],
    ),
    valued(
        "--mode",
        None,
        "<mode>",
        Choice(&["text", "json", "rpc", "acp"]),
        &["Output mode: text (default), json, rpc, or acp"],
    ),
    flag("--print", Some("-p"), &["Non-interactive mode: process prompt and exit"]),
    flag("--continue", Some("-c"), &["Continue previous session"]),
    flag("--resume", Some("-r"), &["Select a session to resume"]),
    valued("--session", None, "<path|id>", File, &["Use specific session file or partial UUID"]),
    valued("--session-id", None, "<id>", Text, &["Use exact project session ID, creating it if missing"]),
    valued("--fork", None, "<path|id>", File, &["Fork specific session file or partial UUID into a new session"]),
    valued("--session-dir", None, "<dir>", Dir, &["Directory for session storage and lookup"]),
    flag("--no-session", None, &["Don't save session (ephemeral)"]),
    valued("--name", Some("-n"), "<name>", Text, &["Set session display name"]),
    valued(
        "--models",
        None,
        "<patterns>",
        Model,
        &[
            "Comma-separated model patterns for Ctrl+P cycling",
            "Supports globs (anthropic/*, *sonnet*) and fuzzy matching",
        ],
    ),
    flag("--no-tools", Some("-nt"), &["Disable all tools by default (built-in and extension)"]),
    flag(
        "--no-builtin-tools",
        Some("-nbt"),
        &["Disable built-in tools by default but keep extension/custom tools enabled"],
    ),
    valued(
        "--tools",
        Some("-t"),
        "<tools>",
        Text,
        &[
            "Comma-separated allowlist of tool names or patterns (*) to enable",
            "Keeps MCP tools unless an entry starts with mcp__",
            "Only +name/-name entries add to or remove from the defaults",
        ],
    ),
    valued(
        "--exclude-tools",
        Some("-xt"),
        "<tools>",
        Text,
        &[
            "Comma-separated denylist of tool names or patterns (*) to disable",
            "Applies to all tools, MCP tools included",
        ],
    ),
    valued(
        "--thinking",
        None,
        "<level>",
        Choice(THINKING_LEVELS),
        &["Set thinking level: off, minimal, low, medium, high, xhigh, max"],
    ),
    valued(
        "--extension",
        Some("-e"),
        "<path>",
        File,
        &["Load an extension file or builtin:<name> (can be used multiple times)"],
    ),
    flag(
        "--no-extensions",
        Some("-ne"),
        &["Disable extension discovery and built-in extensions (explicit -e paths still work)"],
    ),
    flag("--no-mcp", None, &["Disable built-in MCP support: no servers connect and no MCP tools"]),
    valued("--skill", None, "<path>", File, &["Load a skill file or directory (can be used multiple times)"]),
    flag("--no-skills", Some("-ns"), &["Disable skills discovery and loading"]),
    valued(
        "--prompt-template",
        None,
        "<path>",
        File,
        &["Load a prompt template file or directory (can be used multiple times)"],
    ),
    flag("--no-prompt-templates", Some("-np"), &["Disable prompt template discovery and loading"]),
    valued("--theme", None, "<path>", File, &["Load a theme file or directory (can be used multiple times)"]),
    valued("--use-theme", None, "<name[/name]>", Text, &["Set the interactive theme for this run"]),
    flag("--no-themes", None, &["Disable theme discovery and loading"]),
    flag("--no-context-files", Some("-nc"), &["Disable AGENTS.md and CLAUDE.md discovery and loading"]),
    valued(
        "--export",
        None,
        "<file>",
        File,
        &["Export session file to HTML and exit", "An output path ending in .md writes Markdown (pi-rust extension)"],
    ),
    valued("--list-models", None, "[search]", ValueKind::None, &["List available models (with optional fuzzy search)"]),
    flag("--verbose", None, &["Force verbose startup (overrides quietStartup setting)"]),
    valued(
        "--tui-mode",
        None,
        "<mode>",
        Choice(&["fullscreen", "regular"]),
        &["TUI mode: fullscreen (default) or regular"],
    ),
    flag("--approve", Some("-a"), &["Trust project-local files for this run"]),
    flag("--no-approve", Some("-na"), &["Ignore project-local files for this run"]),
    flag("--offline", None, &["Disable startup network operations (same as PI_OFFLINE=1)"]),
    flag("--", None, &["End option parsing; treat remaining arguments as messages/files"]),
    flag("--help", Some("-h"), &["Show this help"]),
    flag("--version", Some("-v"), &["Show version number"]),
];

/// The subcommands of `pi --help`, in order.
pub const CLI_COMMANDS: &[CliCommand] = &[
    command("install", "<source> [-l]", "Install extension source and add to settings", PACKAGE),
    command("remove", "<source> [-l]", "Remove extension source from settings", PACKAGE),
    command("uninstall", "<source> [-l]", "Alias for remove", PACKAGE),
    command(
        "update",
        "[source|self|pi]",
        "Update pi, extensions, or model catalogs",
        &[
            "self",
            "pi",
            "--self",
            "--extensions",
            "--models",
            "--all",
            "--extension",
            "--force",
            "-a",
            "--approve",
            "-na",
            "--no-approve",
        ],
    ),
    command("list", "", "List installed extensions from settings", TRUST),
    command("config", "[-l]", "Open TUI to enable/disable package resources (Tab switches scope)", &["-l", "--local"]),
    command(
        "auth",
        "<command>",
        "Print credentials or check provider readiness",
        &[
            "check",
            "print-api-key",
            "print-bearer-token",
            "--provider",
            "--model",
            "--json",
            "--credentials",
            "--no-refresh",
            "--min-expiry",
        ],
    ),
    command(
        "mcp",
        "<command>",
        "Check MCP servers, sign in to or out of OAuth servers",
        &[
            "add",
            "remove",
            "list",
            "login",
            "logout",
            "-l",
            "--local",
            "--json",
            "--url",
            "--env",
            "--cwd",
            "--header",
            "--exposure",
            "--description",
            "--timeout",
        ],
    ),
    command(
        "completion",
        "<shell>",
        "Print a shell completion script (bash, zsh, fish, powershell)",
        COMPLETION_SHELLS,
    ),
    command("doctor", "[--json]", "Check the install, config, credentials, MCP servers, and terminal", &["--json"]),
];

/// The options `pi completion` offers: every option but `--`.
pub fn completable_options() -> impl Iterator<Item = &'static CliOption> {
    CLI_OPTIONS.iter().filter(|option| !option.is_separator())
}

/// Every flag spelling (long and short) of the completable options.
pub fn option_words() -> Vec<&'static str> {
    completable_options().flat_map(|option| std::iter::once(option.long).chain(option.short)).collect()
}
