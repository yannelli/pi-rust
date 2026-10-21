use super::shells::fish_flag;
use super::*;
use crate::cli::table::{CLI_OPTIONS, option_words};
use crate::cli::{help_text, parse_args};

fn run(args: &[&str]) -> CommandOutput {
    let raw: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    run_completion_command(&raw, Path::new("/nonexistent-agent-dir")).expect("a completion command")
}

#[test]
fn the_parser_and_the_help_know_every_option_of_the_table() {
    let help = help_text();
    for option in completable_options() {
        let value = match option.kind {
            ValueKind::Choice(choices) => choices[0],
            _ if matches!(option.long, "--max-turns" | "--max-cost") => "1",
            _ => "read",
        };
        for spelling in spellings(option) {
            let mut args = vec![spelling.to_string()];
            if option.takes_value() {
                args.push(value.to_string());
            }
            let parsed = parse_args(&args);
            assert!(parsed.errors.is_empty() && parsed.unknown_flags.is_empty(), "{spelling}: {parsed:?}");
        }
        assert!(help.contains(&format!("\n  {}", option.long)), "help lacks {}", option.long);
    }
    for command in CLI_COMMANDS {
        assert!(help.contains(&format!("\n  {APP_NAME} {} ", command.name)), "help lacks {}", command.name);
    }
    assert!(CLI_OPTIONS.iter().any(|option| option.is_separator()));
}

#[test]
fn every_script_offers_every_option_and_command() {
    for shell in COMPLETION_SHELLS {
        let script = run(&["completion", *shell]).stdout;
        for word in option_words() {
            let expected = if *shell == "fish" { fish_flag(word) } else { word.to_string() };
            assert!(script.contains(&expected), "{shell} script lacks {expected}");
        }
        for command in CLI_COMMANDS {
            assert!(script.contains(command.name), "{shell} script lacks {}", command.name);
        }
        assert!(script.contains("completion --values"), "{shell}");
    }
}

#[test]
fn fish_flags_use_the_long_short_and_old_forms() {
    assert_eq!(fish_flag("--model"), "-l model");
    assert_eq!(fish_flag("-p"), "-s p");
    assert_eq!(fish_flag("-nt"), "-o nt");
}

#[test]
fn values_list_providers_and_models_from_the_catalog() {
    let providers = completion_values("providers", Path::new("/nonexistent-agent-dir")).unwrap();
    assert!(providers.iter().any(|provider| provider == "anthropic"), "{providers:?}");
    let models = completion_values("models", Path::new("/nonexistent-agent-dir")).unwrap();
    assert!(models.iter().any(|model| model.starts_with("anthropic/")));
    assert!(completion_values("themes", Path::new("/nonexistent-agent-dir")).is_none());
}

#[test]
fn usage_errors_and_help() {
    assert_eq!(run(&["completion"]).code, 1);
    let unknown = run(&["completion", "tcsh"]);
    assert!(unknown.code == 1 && unknown.stderr.contains("Unknown shell \"tcsh\""), "{unknown:?}");
    assert_eq!(run(&["completion", "bash", "zsh"]).code, 1);
    let help = run(&["completion", "--help"]);
    let install = format!("eval \"$({APP_NAME} completion bash)\"");
    assert!(help.code == 0 && help.stdout.contains(&install), "{}", help.stdout);
    assert!(run_completion_command(&["doctor".to_string()], Path::new("/")).is_none());
}
