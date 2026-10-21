//! `pi --help` text, following upstream `printHelp`.

use super::table::{CLI_COMMANDS, CLI_OPTIONS};
use crate::resources::brand::{APP_NAME, CONFIG_DIR_NAME, agent_dir_env_var, session_dir_env_var};

/// An extension flag for the `Extension CLI Flags` section: name, `string` or `boolean`,
/// description, and the registering extension.
pub struct HelpFlag {
    pub name: String,
    pub kind: String,
    pub description: Option<String>,
    pub extension_path: String,
}

/// `  pi install <source> [-l]     Install ...` lines from the command table.
fn commands_section() -> String {
    let mut text = String::new();
    for command in CLI_COMMANDS {
        let usage = format!("{} {}", command.name, command.usage);
        text.push_str(&format!("  {APP_NAME} {:<26}{}\n", usage.trim_end(), command.help));
    }
    let names: Vec<&str> = CLI_COMMANDS.iter().map(|command| command.name).collect();
    text.push_str(&format!("  {APP_NAME} {:<26}Show help for {}\n", "<command> --help", names.join("/")));
    text
}

/// `  --name, -n <name>   Description` lines from the option table.
fn options_section() -> String {
    let mut text = String::new();
    for option in CLI_OPTIONS {
        let mut usage = option.long.to_string();
        if let Some(short) = option.short {
            usage.push_str(&format!(", {short}"));
        }
        if let Some(value) = option.value {
            usage.push_str(&format!(" {value}"));
        }
        for (index, line) in option.help.iter().enumerate() {
            let first = if index == 0 { usage.as_str() } else { "" };
            text.push_str(&format!("  {first:<31}{line}\n"));
        }
    }
    text
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
{commands}
Options:
{options}
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
        commands = commands_section(),
        options = options_section(),
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
