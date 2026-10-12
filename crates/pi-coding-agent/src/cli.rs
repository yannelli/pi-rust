//! Command-line parsing. Flags match the TypeScript pi where this port supports
//! the feature, including its multi-letter short forms such as `-nt`.

use pi_ai::ThinkingLevel;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputMode {
    #[default]
    Text,
    Json,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Args {
    pub help: bool,
    pub version: bool,
    pub mode: OutputMode,
    pub print: bool,
    pub continue_session: bool,
    pub resume: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub no_session: bool,
    pub session: Option<String>,
    pub session_dir: Option<String>,
    pub no_tools: bool,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    pub thinking: Option<ThinkingLevel>,
    pub no_context_files: bool,
    /// `Some(search)` lists models; an empty search lists all.
    pub list_models: Option<String>,
    pub messages: Vec<String>,
    /// Paths given as `@path`.
    pub file_args: Vec<String>,
    pub errors: Vec<String>,
}

fn split_list(value: &str) -> Vec<String> {
    value.split(',').map(str::trim).filter(|item| !item.is_empty()).map(String::from).collect()
}

pub fn parse_args(raw: &[String]) -> Args {
    let mut args = Args::default();
    let mut index = 0;
    let value = |index: &mut usize, flag: &str, errors: &mut Vec<String>| -> Option<String> {
        match raw.get(*index + 1) {
            Some(next) => {
                *index += 1;
                Some(next.clone())
            }
            None => {
                errors.push(format!("{flag} requires a value"));
                None
            }
        }
    };
    while index < raw.len() {
        let arg = raw[index].as_str();
        match arg {
            "--" => {
                for positional in &raw[index + 1..] {
                    match positional.strip_prefix('@') {
                        Some(path) => args.file_args.push(path.into()),
                        None => args.messages.push(positional.clone()),
                    }
                }
                break;
            }
            "-h" | "--help" => args.help = true,
            "-v" | "--version" => args.version = true,
            "--mode" => match value(&mut index, arg, &mut args.errors).as_deref() {
                Some("text") => args.mode = OutputMode::Text,
                Some("json") => args.mode = OutputMode::Json,
                Some(other) => args.errors.push(format!("--mode must be text or json, got {other}")),
                None => {}
            },
            "-c" | "--continue" => args.continue_session = true,
            "-r" | "--resume" => args.resume = true,
            "--provider" => args.provider = value(&mut index, arg, &mut args.errors),
            "--model" => args.model = value(&mut index, arg, &mut args.errors),
            "--api-key" => args.api_key = value(&mut index, arg, &mut args.errors),
            "--system-prompt" => args.system_prompt = value(&mut index, arg, &mut args.errors),
            "--append-system-prompt" => args.append_system_prompt = value(&mut index, arg, &mut args.errors),
            "--no-session" => args.no_session = true,
            "--session" => args.session = value(&mut index, arg, &mut args.errors),
            "--session-dir" => args.session_dir = value(&mut index, arg, &mut args.errors),
            "-nt" | "--no-tools" => args.no_tools = true,
            "-t" | "--tools" => args.tools = value(&mut index, arg, &mut args.errors).map(|list| split_list(&list)),
            "-xt" | "--exclude-tools" => {
                if let Some(list) = value(&mut index, arg, &mut args.errors) {
                    args.exclude_tools.extend(split_list(&list));
                }
            }
            "--thinking" => {
                if let Some(level) = value(&mut index, arg, &mut args.errors) {
                    match level.parse() {
                        Ok(level) => args.thinking = Some(level),
                        Err(error) => args.errors.push(error),
                    }
                }
            }
            "-p" | "--print" => {
                args.print = true;
                if let Some(next) = raw.get(index + 1)
                    && !next.starts_with('@')
                    && (!next.starts_with('-') || next.starts_with("---"))
                {
                    args.messages.push(next.clone());
                    index += 1;
                }
            }
            "-nc" | "--no-context-files" => args.no_context_files = true,
            "--list-models" => {
                let search = raw.get(index + 1).filter(|next| !next.starts_with('-'));
                args.list_models = Some(search.cloned().unwrap_or_default());
                if search.is_some() {
                    index += 1;
                }
            }
            _ if arg.starts_with('@') => args.file_args.push(arg[1..].into()),
            _ if arg.starts_with('-') => args.errors.push(format!("Unknown option: {arg}")),
            _ => args.messages.push(arg.into()),
        }
        index += 1;
    }
    args
}

pub fn help_text() -> String {
    format!(
        "pi {} - AI coding agent (Rust port)

Usage: pi [options] [@files...] [messages...]

Options:
  -p, --print [message]        Run non-interactively and print the final response
      --mode text|json         Output mode for --print (json prints one event per line)
  -c, --continue               Continue the most recent session in this directory
  -r, --resume                 Pick a session to resume
      --session <path>         Open a session file
      --session-dir <dir>      Directory for session files
      --no-session             Do not save the session
      --provider <name>        Provider, for example anthropic or openai
      --model <pattern>        Model id, provider/id, or search pattern
      --api-key <key>          API key for this run
      --thinking <level>       off, minimal, low, medium, high, xhigh
      --system-prompt <text>   Replace the default system prompt
      --append-system-prompt <text>
                               Append text to the system prompt
  -t, --tools <a,b>            Enable only these tools (read,bash,edit,write,grep,find,ls)
  -xt, --exclude-tools <a,b>   Disable these tools
  -nt, --no-tools              Disable all tools
  -nc, --no-context-files      Do not load AGENTS.md or CLAUDE.md files
      --list-models [search]   List available models
  -h, --help                   Show this help
  -v, --version                Show the version

Files given as @path are added to the first message; images are attached.

Interactive commands: /model [pattern], /thinking [level], /compact [focus], /new,
/session, /resume, /help, /quit. Ctrl+C aborts a running response; press it twice to exit.

Environment: ANTHROPIC_API_KEY, OPENAI_API_KEY, and other provider keys;
PI_CODING_AGENT_DIR overrides ~/.pi/agent.",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Args {
        parse_args(&line.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn parses_upstream_flags() {
        let args = parse("-p hello --model anthropic/claude --thinking high -t read,bash -xt bash -nc @a.txt world");
        assert!(args.print && args.no_context_files);
        assert_eq!(args.messages, ["hello", "world"]);
        assert_eq!(args.model.as_deref(), Some("anthropic/claude"));
        assert_eq!(args.thinking, Some(ThinkingLevel::High));
        assert_eq!(args.tools, Some(vec!["read".to_string(), "bash".to_string()]));
        assert_eq!(args.exclude_tools, ["bash"]);
        assert_eq!(args.file_args, ["a.txt"]);
        assert!(args.errors.is_empty());
    }

    #[test]
    fn reports_bad_input() {
        let args = parse("--thinking huge --bogus --mode xml --model");
        assert_eq!(args.errors.len(), 4, "{:?}", args.errors);
        assert_eq!(parse("--list-models").list_models.as_deref(), Some(""));
        assert_eq!(parse("--list-models son").list_models.as_deref(), Some("son"));
        assert_eq!(parse("-- -p @x").messages, ["-p"]);
    }
}
