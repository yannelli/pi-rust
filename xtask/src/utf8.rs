//! `cargo xtask utf8-asm` and `cargo xtask utf8-bench` for the UTF-8 validator in `crates/pi-durable/src/env/utf8.rs`.
//! Its kernel beats `std` only where the compiler turns the kernel's loops into vector code. The assembly check compiles
//! the file for each release target and fails when one of those loops is scalar; the benchmark times it on this machine.

use std::path::{Path, PathBuf};
use std::process::Command;

const SOURCE: &str = "crates/pi-durable/src/env/utf8.rs";

/// The kernel's loops, by a part of their function's v0 symbol name: the largest-byte pass and the three rule sets.
const LOOPS: [(&str, &str); 4] = [
    ("largest byte", "6kernel12valid_prefix"),
    ("2-byte rules", "6kernel12block_breaksKj2_"),
    ("3-byte rules", "6kernel12block_breaksKj3_"),
    ("4-byte rules", "6kernel12block_breaksKj4_"),
];

/// An innermost loop of a function: its instruction count and how many of those use vector registers.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Loop {
    instructions: usize,
    vector: usize,
}

impl Loop {
    fn is_vector(&self) -> bool {
        self.instructions >= 4 && self.vector * 2 >= self.instructions
    }
}

pub fn asm(args: &[String], root: &Path) -> Result<(), String> {
    let (mut targets, flags) = options(args)?;
    if targets.is_empty() {
        targets = release_targets(root)?;
    }
    let dir = output_dir(root)?;
    println!("{}", rustc_version()?);
    let mut failed = Vec::new();
    for target in &targets {
        let listing = dir.join(format!("{target}.s"));
        let output = rustc()
            .current_dir(root)
            .args(["--edition=2024", "--crate-type=lib", "--target", target])
            .args(["-C", "opt-level=3", "-C", "codegen-units=1", "-C", "symbol-mangling-version=v0"])
            .arg(format!("--emit=asm={}", listing.display()))
            .args(&flags)
            .arg(SOURCE)
            .output()
            .map_err(|error| format!("rustc: {error}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let hint = if stderr.contains("may not be installed") {
                format!("install it with `rustup target add {target}`")
            } else {
                stderr.lines().take(5).collect::<Vec<_>>().join("\n    ")
            };
            println!("FAILED  {target}: rustc failed: {hint}");
            failed.push(target.as_str());
            continue;
        }
        if !(target.starts_with("x86_64") || target.starts_with("aarch64")) {
            println!("ok      {target}: no kernel on this architecture; the DFA or `std` validates");
            continue;
        }
        let text = std::fs::read_to_string(&listing).map_err(|error| format!("{}: {error}", listing.display()))?;
        let functions = functions(&text, target.starts_with("x86_64"));
        let mut ok = true;
        let mut parts = Vec::new();
        for (name, symbol) in LOOPS {
            let loops = functions.iter().find(|(label, _)| names(label, symbol)).map(|(_, loops)| loops);
            let widest = loops.and_then(|loops| loops.iter().filter(|l| l.is_vector()).max_by_key(|l| l.instructions));
            parts.push(match (loops, widest) {
                (_, Some(widest)) => format!("{name} {} ({} vector)", widest.instructions, widest.vector),
                (Some(_), None) => format!("{name} SCALAR"),
                (None, None) => format!("{name} NOT FOUND"),
            });
            ok &= widest.is_some();
        }
        println!("{:<8}{target}: {}", if ok { "ok" } else { "FAILED" }, parts.join(", "));
        if !ok {
            failed.push(target.as_str());
        }
    }
    println!("Instructions per vector loop; listings in {}", dir.display());
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the UTF-8 kernel has no vector loop for {}; see docs/agents/ci-triage.md#utf-8-kernel-assembly-check",
            failed.join(", ")
        ))
    }
}

pub fn bench(args: &[String], root: &Path) -> Result<(), String> {
    let (targets, flags) = options(args)?;
    if !targets.is_empty() {
        return Err("utf8-bench runs on this machine and takes no --target".into());
    }
    let binary = output_dir(root)?.join(format!("utf8-bench{}", std::env::consts::EXE_SUFFIX));
    let status = rustc()
        .current_dir(root)
        .args(["--edition=2024", "--test", "-C", "opt-level=3", "-C", "codegen-units=1", "-o"])
        .arg(&binary)
        .args(&flags)
        .arg(SOURCE)
        .status()
        .map_err(|error| format!("rustc: {error}"))?;
    if !status.success() {
        return Err("rustc failed".into());
    }
    let status = Command::new(&binary)
        .args(["--ignored", "--exact", "tests::benchmark", "--nocapture"])
        .status()
        .map_err(|error| format!("{}: {error}", binary.display()))?;
    if status.success() { Ok(()) } else { Err("the benchmark failed".into()) }
}

/// `--target <triple>` options before `--`, and the rustc flags after it.
fn options(args: &[String]) -> Result<(Vec<String>, Vec<String>), String> {
    let (own, flags) = match args.iter().position(|arg| arg == "--") {
        Some(index) => (&args[..index], &args[index + 1..]),
        None => (args, &[][..]),
    };
    let mut targets = Vec::new();
    let mut own = own.iter();
    while let Some(arg) = own.next() {
        match arg.as_str() {
            "--target" => targets.push(own.next().ok_or("--target needs a target triple")?.clone()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok((targets, flags.to_vec()))
}

/// The `target:` entries of the Release workflow's build matrices.
fn release_targets(root: &Path) -> Result<Vec<String>, String> {
    let path = root.join(".github/workflows/release.yml");
    let workflow = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let targets: Vec<String> = workflow
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- { target: "))
        .filter_map(|rest| rest.split([',', ' ', '}']).next())
        .map(String::from)
        .collect();
    if targets.is_empty() { Err(format!("no targets in {}", path.display())) } else { Ok(targets) }
}

fn rustc() -> Command {
    Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
}

fn rustc_version() -> Result<String, String> {
    let output = rustc().arg("--version").output().map_err(|error| format!("rustc: {error}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn output_dir(root: &Path) -> Result<PathBuf, String> {
    let dir = root.join("target/xtask-utf8");
    std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    Ok(dir)
}

/// Whether a symbol names the function `symbol` itself, not a closure or another function whose name it starts.
fn names(label: &str, symbol: &str) -> bool {
    label.find(symbol).is_some_and(|start| matches!(label[start + symbol.len()..].chars().next(), None | Some('E')))
}

/// A function's instructions, and its local labels with the index of the instruction that follows each.
type Body<'a> = (Vec<&'a str>, Vec<(&'a str, usize)>);

/// Each function of an assembly listing with the innermost loops of its body. A function runs from its symbol to the
/// next label that is not local.
fn functions(listing: &str, x86: bool) -> Vec<(&str, Vec<Loop>)> {
    let mut functions: Vec<(&str, Body)> = Vec::new();
    for line in listing.lines() {
        let text = strip_comment(line, x86);
        if !line.starts_with(char::is_whitespace)
            && let Some((label, _)) = text.split_once(':')
            && !label.is_empty()
            && !label.contains(char::is_whitespace)
        {
            if label.starts_with(['.', 'L', 'l', '$']) {
                if let Some((_, (instructions, labels))) = functions.last_mut() {
                    labels.push((label, instructions.len()));
                }
            } else {
                functions.push((label, (Vec::new(), Vec::new())));
            }
            continue;
        }
        let instruction = text.trim();
        if !instruction.is_empty()
            && !instruction.starts_with('.')
            && let Some((_, (instructions, _))) = functions.last_mut()
        {
            instructions.push(instruction);
        }
    }
    functions
        .into_iter()
        .map(|(name, (instructions, labels))| (name, innermost_loops(&instructions, &labels, x86)))
        .collect()
}

fn strip_comment(line: &str, x86: bool) -> &str {
    let end = if x86 { line.find('#') } else { [line.find("//"), line.find(';')].into_iter().flatten().min() };
    &line[..end.unwrap_or(line.len())]
}

/// Loops closed by a branch back to a label, without the loops that hold another one.
fn innermost_loops(instructions: &[&str], labels: &[(&str, usize)], x86: bool) -> Vec<Loop> {
    let mut spans = Vec::new();
    for (end, instruction) in instructions.iter().enumerate() {
        let mut words = instruction.split(|c: char| c.is_whitespace() || c == ',').filter(|word| !word.is_empty());
        let mnemonic = words.next().unwrap_or_default();
        let branch = if x86 {
            mnemonic.starts_with('j')
        } else {
            mnemonic == "b" || mnemonic.starts_with("b.") || matches!(mnemonic, "cbz" | "cbnz" | "tbz" | "tbnz")
        };
        let target = words.next_back().unwrap_or_default();
        if branch
            && let Some((_, start)) = labels.iter().find(|(label, _)| *label == target)
            && *start <= end
        {
            spans.push((*start, end));
        }
    }
    let inner =
        |span: &(usize, usize)| !spans.iter().any(|other| other != span && span.0 <= other.0 && other.1 <= span.1);
    spans
        .iter()
        .filter(|span| inner(span))
        .map(|(start, end)| {
            let body = &instructions[*start..=*end];
            Loop { instructions: body.len(), vector: body.iter().filter(|text| uses_vector(text, x86)).count() }
        })
        .collect()
}

/// Whether an instruction has a vector register operand: x86 `xmm`, `ymm`, `zmm`; ARM `v`, `q`, or `d`.
fn uses_vector(instruction: &str, x86: bool) -> bool {
    let operands = instruction.split_once(char::is_whitespace).map_or("", |(_, operands)| operands);
    let prefixes: &[&str] = if x86 { &["xmm", "ymm", "zmm"] } else { &["v", "q", "d"] };
    operands.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.')).any(|word| {
        let register = word.split('.').next().unwrap_or_default();
        prefixes
            .iter()
            .filter_map(|prefix| register.strip_prefix(prefix))
            .any(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_innermost_loops_and_their_vector_instructions() {
        let arm = "_RNvNtCs1_4utf86kernel12valid_prefix:\n\tcbz\tx1, LBB0_3\nLBB0_1: ; outer\n\tldrb\tw8, [x0]\nLBB0_2:\n\
                   \tldp\tq0, q1, [x0], #32\n\tumax.16b\tv0, v0, v1\n\tsubs\tx1, x1, #32\n\tb.ne\tLBB0_2\n\tb\tLBB0_1\n\
                   LBB0_3:\n\tret\n_RNvNtCs1_4utf86kernel12valid_prefix0:\n\tret\n";
        let found = functions(arm, false);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].1, vec![Loop { instructions: 4, vector: 2 }]);
        assert!(names(found[0].0, LOOPS[0].1) && !names(found[1].0, LOOPS[0].1));
        let x86 = "_RINvNtCs1_4utf86kernel12block_breaksKj2_EB4_:\n.LBB1_1:\t# loop\n\tmovdqu\txmm0, xmmword ptr [rdi]\n\
                   \tadd\trdi, 16 # xmm9\n\tcmp\trdi, rsi\n\tjne\t.LBB1_1\n\tret\n";
        let found = functions(x86, true);
        assert!(names(found[0].0, LOOPS[1].1) && !names(found[0].0, LOOPS[2].1));
        assert_eq!(found[0].1, vec![Loop { instructions: 4, vector: 1 }]);
        assert!(!found[0].1[0].is_vector());
    }
}
