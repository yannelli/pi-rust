//! A `node`-style command line on pi-js, for children made by `child_process.fork` (or a
//! spawn of `process.execPath`). Host binaries call [`child_main`] first thing in `main`.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::runtime::{JsRuntime, JsRuntimeOptions};
use crate::{JsError, OutputStream, StdOutput};

/// Set in a child's environment when the parent wants it to run JavaScript like `node`.
pub const FORK_ENV: &str = "PI_JS_FORK";

/// Run the Node-style command line when this process is such a child; `None` otherwise.
pub fn child_main() -> Option<i32> {
    std::env::var_os(FORK_ENV)?;
    Some(run_node_cli(std::env::args().skip(1).collect()))
}

#[derive(Debug, Default, PartialEq)]
struct NodeCli {
    exec_argv: Vec<String>,
    eval: Option<(String, bool)>,
    preload: Vec<String>,
    imports: Vec<String>,
    script: Option<String>,
    args: Vec<String>,
}

fn parse(args: Vec<String>) -> NodeCli {
    let mut cli = NodeCli::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag.to_string(), Some(value.to_string())),
            _ => (arg.clone(), None),
        };
        let takes_value = matches!(flag.as_str(), "-e" | "--eval" | "-p" | "--print" | "-r" | "--require" | "--import");
        if takes_value {
            let value = inline.or_else(|| iter.next()).unwrap_or_default();
            cli.exec_argv.extend([flag.clone(), value.clone()]);
            match flag.as_str() {
                "-e" | "--eval" => cli.eval = Some((value, false)),
                "-p" | "--print" => cli.eval = Some((value, true)),
                "-r" | "--require" => cli.preload.push(value),
                _ => cli.imports.push(value),
            }
            if cli.eval.is_some() {
                cli.args = iter.by_ref().skip_while(|arg| arg == "--").collect();
                break;
            }
        } else if arg == "--" {
            cli.script = iter.next();
            cli.args = iter.by_ref().collect();
        } else if arg.starts_with('-') && arg != "-" {
            cli.exec_argv.push(arg);
        } else {
            cli.script = Some(arg);
            cli.args = iter.by_ref().collect();
        }
    }
    cli
}

/// Run `node [options] [script | -e code | -p code] [args]` and return the exit code.
pub fn run_node_cli(args: Vec<String>) -> i32 {
    let cli = parse(args);
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let exe = std::env::current_exe().map(|path| path.display().to_string()).unwrap_or_else(|_| "pi".into());
    let script = cli.script.as_ref().filter(|script| script.as_str() != "-").map(|script| {
        let path = cwd.join(script);
        crate::resolve::canonical(&path).display().to_string()
    });
    let mut options = JsRuntimeOptions::new(&cwd);
    options.argv = std::iter::once(exe).chain(script.clone()).chain(cli.args.iter().cloned()).collect();
    options.env = Some(std::env::vars().filter(|(key, _)| key != FORK_ENV).collect::<BTreeMap<_, _>>());
    options.on_uncaught = Some(Arc::new(|_kind, error: &JsError| {
        crate::OutputSink::write(
            &StdOutput,
            OutputStream::Stderr,
            format!("{}\n", error.display_with_stack()).as_bytes(),
        );
        std::process::exit(1);
    }));
    let runtime = match JsRuntime::new(options) {
        Ok(runtime) => runtime,
        Err(error) => return fail(&error),
    };
    let json = |text: &str| serde_json::to_string(text).unwrap_or_default();
    let base = json(&cwd.join("[eval]").display().to_string());
    let result = futures::executor::block_on(async {
        let exec_argv = serde_json::to_string(&cli.exec_argv).unwrap_or_default();
        runtime
            .eval(format!(
                "process.execArgv = {exec_argv}; globalThis.require ??= __pi_internal.createRequire({base}); void 0"
            ))
            .await?;
        for module in &cli.preload {
            runtime.eval(format!("require({}); void 0", json(module))).await?;
        }
        for module in &cli.imports {
            runtime.import(module.clone()).await?;
        }
        let code = match (&cli.eval, &script) {
            (Some((code, print)), _) => Some((code.clone(), *print)),
            (None, Some(script)) => {
                runtime.import(script.clone()).await?;
                None
            }
            (None, None) => Some((std::io::read_to_string(std::io::stdin()).unwrap_or_default(), false)),
        };
        if let Some((code, print)) = code {
            let run = format!("(0, eval)({})", json(&code));
            let source = if print { format!("console.log({run}); void 0") } else { format!("{run}; void 0") };
            runtime.eval(source).await?;
        }
        runtime.idle().await?;
        runtime.eval("if (process.listenerCount('beforeExit') > 0) process.emit('beforeExit', process.exitCode ?? 0); void 0").await?;
        runtime.idle().await?;
        runtime.eval("process.exit(); void 0").await.map(|_| ())
    });
    match result {
        Ok(()) => 0,
        Err(error) => fail(&error),
    }
}

fn fail(error: &JsError) -> i32 {
    crate::OutputSink::write(&StdOutput, OutputStream::Stderr, format!("{}\n", error.display_with_stack()).as_bytes());
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn parses_node_command_lines() {
        let cli = parse(args(&["--no-warnings", "-r", "./pre.js", "main.js", "--flag", "x"]));
        assert_eq!(cli.exec_argv, args(&["--no-warnings", "-r", "./pre.js"]));
        assert_eq!(
            (cli.preload, cli.script, cli.args),
            (args(&["./pre.js"]), Some("main.js".into()), args(&["--flag", "x"]))
        );
        let eval = parse(args(&["-p", "1 + 1", "a"]));
        assert_eq!((eval.eval, eval.args), (Some(("1 + 1".into(), true)), args(&["a"])));
        let inline = parse(args(&["--eval=x()", "--", "b"]));
        assert_eq!((inline.eval, inline.args), (Some(("x()".into(), false)), args(&["b"])));
    }
}
