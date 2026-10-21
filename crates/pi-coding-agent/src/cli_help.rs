//! `pi --help` text, following upstream `printHelp`.

use crate::resources::brand::{APP_NAME, CONFIG_DIR_NAME, agent_dir_env_var, session_dir_env_var};

/// An extension flag for the `Extension CLI Flags` section: name, `string` or `boolean`,
/// description, and the registering extension.
pub struct HelpFlag {
    pub name: String,
    pub kind: String,
    pub description: Option<String>,
    pub extension_path: String,
}

pub fn help_text() -> String {
    help_text_with_flags(&[])
}

/// `--help` with the flags loaded extensions registered.
pub fn help_text_with_flags(flags: &[HelpFlag]) -> String {
    let mut extension_flags = String::new();
    if !flags.is_empty() {
        extension_flags.push_str("\nExtension CLI Flags:\n");
        for flag in flags {
            let value = if flag.kind == "string" { " <value>" } else { "" };
            let usage = format!("  --{}{value}", flag.name);
            let description =
                flag.description.clone().unwrap_or_else(|| format!("Registered by {}", flag.extension_path));
            extension_flags.push_str(&format!("{usage:<30}{description}\n"));
        }
    }
    format!(
        "{app} - AI coding assistant with read, bash, edit, write tools

Usage:
  {app} [options] [--] [@files...] [messages...]

Commands:
  {app} install <source> [-l]     Install extension source and add to settings
  {app} remove <source> [-l]      Remove extension source from settings
  {app} uninstall <source> [-l]   Alias for remove
  {app} update [source|self|pi]   Update pi, extensions, or model catalogs
  {app} list                      List installed extensions from settings
  {app} config [-l]               Open TUI to enable/disable package resources (Tab switches scope)
  {app} auth <command>            Print credentials or check provider readiness
  {app} mcp <command>             Check MCP servers, sign in to or out of OAuth servers
  {app} <command> --help          Show help for install/remove/uninstall/update/list/config/auth/mcp

Options:
  --provider <name>              Provider to search for --model (requires --model)
  --model <pattern>              Model pattern or ID (supports \"provider/id\" and optional \":<thinking>\")
  --api-key <key>                API key (defaults to env vars)
  --system-prompt <text>         System prompt text or file (default: coding assistant prompt)
  --append-system-prompt <text>  Append text or file contents to the system prompt (can be used multiple times)
  --mode <mode>                  Output mode: text (default), json, rpc, or acp
  --print, -p                    Non-interactive mode: process prompt and exit
  --continue, -c                 Continue previous session
  --resume, -r                   Select a session to resume
  --session <path|id>            Use specific session file or partial UUID
  --session-id <id>              Use exact project session ID, creating it if missing
  --fork <path|id>               Fork specific session file or partial UUID into a new session
  --session-dir <dir>            Directory for session storage and lookup
  --no-session                   Don't save session (ephemeral)
  --name, -n <name>              Set session display name
  --models <patterns>            Comma-separated model patterns for Ctrl+P cycling
                                 Supports globs (anthropic/*, *sonnet*) and fuzzy matching
  --no-tools, -nt                Disable all tools by default (built-in and extension)
  --no-builtin-tools, -nbt       Disable built-in tools by default but keep extension/custom tools enabled
  --tools, -t <tools>            Comma-separated allowlist of tool names or patterns (*) to enable
                                 Keeps MCP tools unless an entry starts with mcp__
                                 Only +name/-name entries add to or remove from the defaults
  --exclude-tools, -xt <tools>   Comma-separated denylist of tool names or patterns (*) to disable
                                 Applies to all tools, MCP tools included
  --thinking <level>             Set thinking level: off, minimal, low, medium, high, xhigh, max
  --extension, -e <path>         Load an extension file or builtin:<name> (can be used multiple times)
  --no-extensions, -ne           Disable extension discovery and built-in extensions (explicit -e paths still work)
  --no-mcp                       Disable built-in MCP support: no servers connect and no MCP tools
  --skill <path>                 Load a skill file or directory (can be used multiple times)
  --no-skills, -ns               Disable skills discovery and loading
  --prompt-template <path>       Load a prompt template file or directory (can be used multiple times)
  --no-prompt-templates, -np     Disable prompt template discovery and loading
  --theme <path>                 Load a theme file or directory (can be used multiple times)
  --use-theme <name[/name]>      Set the interactive theme for this run
  --no-themes                    Disable theme discovery and loading
  --no-context-files, -nc        Disable AGENTS.md and CLAUDE.md discovery and loading
  --export <file>                Export session file to HTML and exit
                                 An output path ending in .md writes Markdown (pi-rust extension)
  --list-models [search]         List available models (with optional fuzzy search)
  --verbose                      Force verbose startup (overrides quietStartup setting)
  --tui-mode <mode>              TUI mode: fullscreen (default) or regular
  --approve, -a                  Trust project-local files for this run
  --no-approve, -na              Ignore project-local files for this run
  --offline                      Disable startup network operations (same as PI_OFFLINE=1)
  --                             End option parsing; treat remaining arguments as messages/files
  --help, -h                     Show this help
  --version, -v                  Show version number

Extensions can register additional flags (e.g., --plan from plan-mode extension).{extension_flags}

Examples:
  # Print a provider API key for an external client
  {app} auth print-api-key --provider openai

  # Print an OAuth bearer token for an external client (refreshes if expired)
  {app} auth print-bearer-token --provider openai-codex

  # Interactive mode
  {app}

  # Interactive mode with initial prompt
  {app} \"List all .ts files in src/\"

  # Include files in initial message
  {app} @prompt.md @image.png \"What color is the sky?\"

  # Non-interactive mode (process and exit)
  {app} -p \"List all .ts files in src/\"

  # Prompt beginning with a dash
  {app} -p -- \"- Summarize these points\"

  # Multiple messages (interactive)
  {app} \"Read package.json\" \"What dependencies do we have?\"

  # Continue previous session
  {app} --continue \"What did we discuss?\"

  # Start a named session
  {app} --name \"Refactor auth module\"

  # Use different model
  {app} --provider openai --model gpt-4o-mini \"Help me refactor this code\"

  # Use model with provider prefix (no --provider needed)
  {app} --model openai/gpt-4o \"Help me refactor this code\"

  # Use model with thinking level shorthand
  {app} --model sonnet:high \"Solve this complex problem\"

  # Limit model cycling to specific models
  {app} --models claude-sonnet,claude-haiku,gpt-4o

  # Limit to a specific provider with glob pattern
  {app} --models \"github-copilot/*\"

  # Cycle models with fixed thinking levels
  {app} --models sonnet:high,haiku:low

  # Start with a specific thinking level
  {app} --thinking high \"Solve this complex problem\"

  # Read-only mode (no file modifications possible)
  {app} --tools read,grep,find,ls -p \"Review the code in src/\"

  # Add codemode to the default tools
  {app} --tools +codemode

  # Codemode with only the tools of one MCP server
  {app} --tools read,bash,codemode,'mcp__radius__*'

  # Disable one tool while keeping the rest available
  {app} --exclude-tools ask_question

  # Export a session file to HTML
  {app} --export ~/{config_dir}/agent/sessions/--path--/session.jsonl
  {app} --export session.jsonl output.html

  # Export a session file to Markdown (pi-rust extension)
  {app} --export session.jsonl output.md

Environment Variables:
  ANTHROPIC_AUTH_TOKEN             - Anthropic bearer auth token
  ANTHROPIC_API_KEY                - Anthropic Claude API key
  ANTHROPIC_OAUTH_TOKEN            - Anthropic OAuth token (alternative to API key)
  ANT_LING_API_KEY                 - Ant Ling API key
  OPENAI_API_KEY                   - OpenAI GPT API key
  AZURE_OPENAI_API_KEY             - Azure OpenAI API key
  DEEPSEEK_API_KEY                 - DeepSeek API key
  NVIDIA_API_KEY                   - NVIDIA NIM API key
  GEMINI_API_KEY                   - Google Gemini API key
  GROQ_API_KEY                     - Groq API key
  CEREBRAS_API_KEY                 - Cerebras API key
  XAI_API_KEY                      - xAI Grok API key
  FIREWORKS_API_KEY                - Fireworks API key
  TOGETHER_API_KEY                 - Together AI API key
  BASETEN_API_KEY                  - Baseten API key
  OPENROUTER_API_KEY               - OpenRouter API key
  AI_GATEWAY_API_KEY               - Vercel AI Gateway API key
  ZAI_API_KEY                      - ZAI Coding Plan API key (Global)
  ZAI_CODING_CN_API_KEY            - ZAI Coding Plan API key (China)
  MISTRAL_API_KEY                  - Mistral API key
  MINIMAX_API_KEY                  - MiniMax API key
  MINIMAX_CN_API_KEY               - MiniMax API key (China)
  MOONSHOT_API_KEY                 - Moonshot AI API key
  OPENCODE_API_KEY                 - OpenCode Zen/OpenCode Go API key
  KIMI_API_KEY                     - Kimi For Coding API key
  META_API_KEY                     - Meta Model API key
  CLOUDFLARE_API_KEY               - Cloudflare API token (Workers AI and AI Gateway)
  QWEN_TOKEN_PLAN_API_KEY          - Qwen Token Plan API key (international region)
  QWEN_TOKEN_PLAN_CN_API_KEY       - Qwen Token Plan API key (China region)
  XIAOMI_API_KEY                   - Xiaomi MiMo API key
  XIAOMI_TOKEN_PLAN_CN_API_KEY     - Xiaomi MiMo Token Plan API key (China region)
  XIAOMI_TOKEN_PLAN_AMS_API_KEY    - Xiaomi MiMo Token Plan API key (Amsterdam region)
  XIAOMI_TOKEN_PLAN_SGP_API_KEY    - Xiaomi MiMo Token Plan API key (Singapore region)
  {agent_dir_env:<32} - Config directory (default: ~/{config_dir}/agent)
  {session_dir_env:<32} - Session storage directory (overridden by --session-dir)
  PI_OFFLINE                       - Disable startup network operations when set to 1/true/yes

Built-in Tool Names:
  read       - Read file contents
  bash       - Execute bash commands
  edit       - Edit files with find/replace
  write      - Write files (creates/overwrites)
  grep       - Search file contents (read-only, off by default)
  find       - Find files by glob pattern (read-only, off by default)
  ls         - List directory contents (read-only, off by default)",
        app = APP_NAME,
        config_dir = CONFIG_DIR_NAME,
        agent_dir_env = agent_dir_env_var(),
        session_dir_env = session_dir_env_var(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_uses_the_brand_and_lists_every_upstream_example() {
        let text = help_text();
        assert!(text.starts_with(&format!("{APP_NAME} - AI coding assistant")), "{text}");
        assert!(text.contains(&format!("\n  {APP_NAME} mcp <command>  ")));
        for example in [
            "# Print an OAuth bearer token for an external client (refreshes if expired)",
            "# Prompt beginning with a dash",
            "# Cycle models with fixed thinking levels",
            "# Codemode with only the tools of one MCP server",
            "--tools read,bash,codemode,'mcp__radius__*'",
            "--export session.jsonl output.html",
            "--export session.jsonl output.md",
        ] {
            assert!(text.contains(example), "missing {example}");
        }
        let agent_dir =
            format!("  {:<32} - Config directory (default: ~/{CONFIG_DIR_NAME}/agent)", agent_dir_env_var());
        assert!(text.contains(&agent_dir));
        assert!(text.contains(&format!("~/{CONFIG_DIR_NAME}/agent/sessions/--path--/session.jsonl")));
        assert!(text.contains(&session_dir_env_var()));
    }
}
