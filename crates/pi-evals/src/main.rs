//! `pi-evals`: run host evals, the documentation comparison, or the in-container runner.

use std::process::ExitCode;

use pi_evals::framework::{Project, list_cases, run_cases, select_cases};
use pi_evals::harness::process_env;

const USAGE: &str = "Usage:
  pi-evals eval [docs options]        Host evals, then the documentation comparison
  pi-evals host [files] [-t pattern]  Host evals on this machine
  pi-evals docs [files] [--provider P --model M] [--runs-per-variant N] [-t pattern]
  pi-evals run --project host|docs [files] [-t pattern] [--json out] [--list out]
  pi-evals container [--discover] ... Image entrypoint

Host and documentation evals need PI_PROVIDER and PI_MODEL.";

struct RunArgs {
    project: Project,
    files: Vec<String>,
    pattern: Option<regex::Regex>,
    json: Option<String>,
    list: Option<String>,
}

fn parse_run_args(args: &[String], default_project: Project) -> anyhow::Result<RunArgs> {
    let mut run = RunArgs { project: default_project, files: Vec::new(), pattern: None, json: None, list: None };
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        let mut value = || {
            index += 1;
            args.get(index).cloned().ok_or_else(|| anyhow::anyhow!("Missing value for {argument}."))
        };
        match argument {
            "--project" => {
                let name = value()?;
                run.project = Project::parse(&name).ok_or_else(|| anyhow::anyhow!("Unknown project: {name}"))?;
            }
            "-t" | "--testNamePattern" => run.pattern = Some(regex::Regex::new(&value()?)?),
            "--json" => run.json = Some(value()?),
            "--list" => run.list = Some(value()?),
            _ if argument.starts_with("--testNamePattern=") => {
                run.pattern = Some(regex::Regex::new(&argument["--testNamePattern=".len()..])?);
            }
            _ if argument.ends_with(".eval.rs") => run.files.push(argument.to_string()),
            _ => anyhow::bail!("Unsupported argument: {argument}"),
        }
        index += 1;
    }
    Ok(run)
}

async fn run_suites(args: &[String], default_project: Project) -> anyhow::Result<bool> {
    let run = parse_run_args(args, default_project)?;
    let suites = pi_evals::evals::all_suites();
    let selected = select_cases(&suites, run.project, &run.files, run.pattern.as_ref());
    if let Some(path) = &run.list {
        std::fs::write(path, serde_json::to_string_pretty(&list_cases(&selected))?)?;
        return Ok(true);
    }
    if selected.is_empty() {
        anyhow::bail!("No eval cases matched the selection.");
    }
    let report = run_cases(&selected).await;
    if let Some(path) = &run.json {
        std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
    }
    Ok(report["success"] == true)
}

async fn main_inner(args: Vec<String>) -> anyhow::Result<bool> {
    let env = process_env();
    let (command, rest) = args.split_first().map_or(("", &[][..]), |(first, rest)| (first.as_str(), rest));
    match command {
        "host" => run_suites(rest, Project::Host).await,
        "run" => run_suites(rest, Project::Host).await,
        "docs" => tokio::task::block_in_place(|| pi_evals::cli::run_docs_comparison(rest, &env)),
        "eval" => {
            if !run_suites(&[], Project::Host).await? {
                return Ok(false);
            }
            tokio::task::block_in_place(|| pi_evals::cli::run_docs_comparison(rest, &env))
        }
        "container" => Ok(pi_evals::container::run_container(rest, &env)? == 0),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(true)
        }
        _ => anyhow::bail!("{USAGE}"),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match main_inner(std::env::args().skip(1).collect()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}
