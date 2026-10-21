use super::*;

fn parse(line: &str) -> Args {
    parse_args(&line.split_whitespace().map(String::from).collect::<Vec<_>>())
}

#[test]
fn parses_upstream_flags() {
    let args = parse("-p hello --model anthropic/claude --thinking max -t read,bash -xt bash -nc @a.txt world");
    assert!(args.print && args.no_context_files);
    assert_eq!(args.messages, ["hello", "world"]);
    assert_eq!(args.model.as_deref(), Some("anthropic/claude"));
    assert_eq!(args.thinking, Some(ThinkingLevel::Max));
    assert_eq!(args.tools, Some(vec!["read".to_string(), "bash".to_string()]));
    assert_eq!(args.exclude_tools, ["bash"]);
    assert_eq!(args.file_args, ["a.txt"]);
    assert!(args.errors.is_empty());
}

#[test]
fn parses_session_resource_and_mode_flags() {
    let args = parse(
        "--session-id abc --fork old.jsonl -n Refactor --models a,,b --export s.jsonl -e ext.ts -e builtin:mcp -ne \
         --no-mcp --skill s1 --skill s2 -ns --prompt-template p -np --theme t --use-theme dark --no-themes \
         --tui-mode regular --verbose -a --offline -nbt --append-system-prompt one --append-system-prompt two \
         --system-prompt sys --mode rpc",
    );
    assert!(args.errors.is_empty(), "{:?}", args.errors);
    assert_eq!(
        (args.session_id.as_deref(), args.fork.as_deref(), args.name.as_deref()),
        (Some("abc"), Some("old.jsonl"), Some("Refactor"))
    );
    assert_eq!(args.models, Some(vec!["a".to_string(), "b".to_string()]));
    assert_eq!(args.export.as_deref(), Some("s.jsonl"));
    assert_eq!(args.extensions, ["ext.ts", "builtin:mcp"]);
    assert!(args.no_extensions && args.no_mcp && args.no_skills && args.no_prompt_templates && args.no_themes);
    assert_eq!((args.skills.len(), args.prompt_templates.len(), args.themes.len()), (2, 1, 1));
    assert_eq!(args.use_theme.as_deref(), Some("dark"));
    assert_eq!(args.tui_mode, Some(TuiMode::Regular));
    assert!(args.verbose && args.offline && args.no_builtin_tools);
    assert_eq!(args.project_trust_override, Some(true));
    assert_eq!(parse("-na").project_trust_override, Some(false));
    assert_eq!(args.append_system_prompt, ["one", "two"]);
    assert_eq!(args.system_prompt.as_deref(), Some("sys"));
    assert_eq!(args.mode, Some(OutputMode::Rpc));
}

#[test]
fn reports_bad_input_like_upstream() {
    let args = parse("--thinking huge --bogus -x --mode xml --model");
    assert_eq!(args.errors.len(), 3, "{:?}", args.errors);
    assert!(args.errors.contains(&"Invalid mode \"xml\". Valid values: text, json, rpc, acp".to_string()));
    assert!(args.warnings[0].contains("Invalid thinking level \"huge\""));
    assert_eq!(args.unknown_flags, [("bogus".to_string(), FlagValue::Bool)]);
    assert_eq!(unknown_flags_error(&args.unknown_flags).unwrap(), "Unknown option: --bogus");
    let flags = parse("--plan --level=3 --out file.txt").unknown_flags;
    assert_eq!(flags[1], ("level".to_string(), FlagValue::String("3".into())));
    assert_eq!(flags[2], ("out".to_string(), FlagValue::String("file.txt".into())));
    assert_eq!(unknown_flags_error(&flags).unwrap(), "Unknown options: --plan, --level, --out");
    assert_eq!(parse("--mode --print").errors, ["--mode requires text, json, rpc, or acp"]);
    let acp = parse("--mode acp --terminal-login");
    assert_eq!((acp.mode, acp.terminal_login), (Some(OutputMode::Acp), true));
    assert!(parse("--tui-mode wide").errors[0].contains("Invalid TUI mode"));
    assert_eq!(parse("--use-theme").errors, ["--use-theme requires a theme name"]);
    assert_eq!(parse("--name").errors, ["--name requires a value"]);
    assert!(parse("-t read,+bash").errors[0].contains("cannot be mixed"));
    assert_eq!(parse("-t +codemode").tools, Some(vec!["+codemode".to_string()]));
    assert_eq!(parse("--list-models").list_models.as_deref(), Some(""));
    assert_eq!(parse("--list-models son").list_models.as_deref(), Some("son"));
    assert_eq!(parse("--list-models @file").list_models.as_deref(), Some(""));
    assert_eq!(parse("-- -p @x").messages, ["-p"]);
    assert_eq!(parse("-p ---yaml").messages, ["---yaml"]);
    assert_eq!(normalize_session_name("  name "), Some("name".into()));
    assert_eq!(normalize_session_name("   "), None);
}

#[test]
fn parses_and_validates_run_limits() {
    let args = parse("--max-turns 10 --max-cost 0.50 --plan -p hi");
    assert!(args.errors.is_empty(), "{:?}", args.errors);
    assert_eq!(args.run_limits, RunLimits { max_turns: Some(10), max_cost: Some(0.5) });
    assert_eq!(args.unknown_flags, [("plan".to_string(), FlagValue::Bool)], "extension flags still pass through");
    let equals = parse("--max-turns=3 --max-cost=0 --level=2");
    assert_eq!(equals.run_limits, RunLimits { max_turns: Some(3), max_cost: Some(0.0) });
    assert_eq!(equals.unknown_flags, [("level".to_string(), FlagValue::String("2".into()))]);
    for (line, error) in [
        ("--max-turns 0", "--max-turns requires a positive integer, got \"0\""),
        ("--max-turns 2.5", "--max-turns requires a positive integer, got \"2.5\""),
        ("--max-turns -1", "--max-turns requires a positive integer, got \"-1\""),
        ("--max-cost -0.1", "--max-cost requires a non-negative number of US dollars, got \"-0.1\""),
        ("--max-cost abc", "--max-cost requires a non-negative number of US dollars, got \"abc\""),
        ("--max-cost inf", "--max-cost requires a non-negative number of US dollars, got \"inf\""),
        ("--max-cost=", "--max-cost requires a non-negative number of US dollars, got \"\""),
        ("--max-turns", "--max-turns requires a value"),
    ] {
        assert_eq!(parse(line).errors, [error], "{line}");
    }
}
