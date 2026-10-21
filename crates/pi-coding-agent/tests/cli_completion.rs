//! `pi completion` through the binary: the scripts parse in their shells (shells that are not
//! installed are skipped), list every option of the table, and ask pi for model values.

mod cli_support;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use cli_support::{isolate, setup};
use pi_coding_agent::cli::table::{CLI_COMMANDS, option_words};

fn run_pi(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    isolate(&mut command, root).args(args).current_dir(root.join("work")).stdin(Stdio::null());
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    root
}

#[test]
fn scripts_list_every_option_and_command_of_the_table() {
    let root = root();
    for shell in ["bash", "zsh", "powershell"] {
        let output = run_pi(root.path(), &["completion", shell]);
        assert!(output.status.success(), "{shell}: {}", String::from_utf8_lossy(&output.stderr));
        let script = stdout(&output);
        for word in option_words() {
            assert!(script.contains(word), "{shell} script lacks {word}");
        }
        for command in CLI_COMMANDS {
            assert!(script.contains(command.name), "{shell} script lacks {}", command.name);
        }
    }
    let fish = stdout(&run_pi(root.path(), &["completion", "fish"]));
    for needle in ["-l model -x -a", "-s p", "-o nt", "-l session-dir", "__fish_seen_subcommand_from auth"] {
        assert!(fish.contains(needle), "fish script lacks {needle}");
    }
}

#[test]
fn values_come_from_the_catalog_and_models_json() {
    let root = root();
    let providers = stdout(&run_pi(root.path(), &["completion", "--values", "providers"]));
    assert!(providers.lines().any(|line| line == "mock") && providers.lines().any(|line| line == "anthropic"));
    let models = stdout(&run_pi(root.path(), &["completion", "--values", "models"]));
    assert!(models.lines().any(|line| line == "mock/mock-model"), "{models}");
    let help = run_pi(root.path(), &["completion", "--help"]);
    assert!(help.status.success() && stdout(&help).contains("~/.config/fish/completions/"));
    assert_eq!(run_pi(root.path(), &["completion", "tcsh"]).status.code(), Some(1));
}

#[cfg(unix)]
mod unix {
    use super::*;

    fn shell_available(shell: &str) -> bool {
        let status = Command::new(shell).args(["-c", "exit 0"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        status.is_ok_and(|status| status.success())
    }

    #[test]
    fn scripts_parse_in_their_shells() {
        let root = root();
        for shell in ["bash", "zsh", "fish"] {
            if !shell_available(shell) {
                eprintln!("skipping {shell} -n: {shell} is not installed");
                continue;
            }
            let path = root.path().join(format!("pi.{shell}"));
            std::fs::write(&path, run_pi(root.path(), &["completion", shell]).stdout).unwrap();
            let check = Command::new(shell).arg("-n").arg(&path).output().unwrap();
            assert!(check.status.success(), "{shell} -n: {}", String::from_utf8_lossy(&check.stderr));
        }
    }

    #[test]
    fn bash_completes_options_subcommands_and_models() {
        let root = root();
        let script = root.path().join("pi.bash");
        std::fs::write(&script, run_pi(root.path(), &["completion", "bash"]).stdout).unwrap();
        let bin = Path::new(env!("CARGO_BIN_EXE_pi")).parent().unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin.to_path_buf())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
        )
        .unwrap();
        let function = format!("_{}_completion", pi_coding_agent::resources::brand::APP_NAME);
        let probe = format!(
            "source '{}'
t() {{ COMP_WORDS=(\"$@\"); COMP_CWORD=$(( ${{#COMP_WORDS[@]}} - 1 )); COMPREPLY=(); {function}; echo \"${{COMPREPLY[*]}}\"; }}
t pi --thin; t pi --thinking x; t pi comp; t pi --model mock/; t pi --provider moc; t pi completion f; t pi auth check --no-r",
            script.display()
        );
        let mut command = Command::new("bash");
        isolate(&mut command, root.path())
            .args(["--norc", "--noprofile", "-c", &probe])
            .env("PATH", path)
            .stdin(Stdio::null());
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let lines: Vec<String> = stdout(&output).lines().map(String::from).collect();
        assert_eq!(lines, ["--thinking", "xhigh", "completion", "mock/mock-model", "mock", "fish", "--no-refresh"]);
    }
}
