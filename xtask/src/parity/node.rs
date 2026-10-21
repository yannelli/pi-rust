//! Section 8: Node built-in module names against `NODE_BUILTINS` in pi-js.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use super::parse::const_array_literals;
use super::scan::Lang;
use super::sections::{build, diff, read_local};

/// Module names from the JSON array `require('module').builtinModules` prints, without `node:`.
pub fn parse_builtins(json: &str) -> Result<BTreeSet<String>, String> {
    let names: Vec<String> = serde_json::from_str(json).map_err(|error| error.to_string())?;
    Ok(names.into_iter().map(|name| name.strip_prefix("node:").map(String::from).unwrap_or(name)).collect())
}

fn node(args: &[&str]) -> Result<String, String> {
    let output = Command::new("node").args(args).output().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => "node is not on PATH".to_string(),
        _ => format!("node: {error}"),
    })?;
    if !output.status.success() {
        return Err(format!("node: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `builtinModules` plus the given names that exist only with the `node:` prefix, such as
/// `node:sea` and `node:test`, which Node 22 leaves out of `builtinModules`.
const BUILTINS_SCRIPT: &str = "const m = require('module'); JSON.stringify([...m.builtinModules, \
     ...process.argv.slice(1).map((n) => 'node:' + n).filter((n) => m.isBuiltin(n))])";

pub fn section(root: &Path) -> super::report::Section {
    build("nodeBuiltins", "Node built-ins", |section| {
        let source = read_local(root, "crates/pi-js/src/resolve/mod.rs")?;
        let local: BTreeSet<String> = const_array_literals(&source, "NODE_BUILTINS", Lang::Rust)
            .ok_or("no NODE_BUILTINS in resolve/mod.rs")?
            .into_iter()
            .collect();
        let mut args = vec!["-p", BUILTINS_SCRIPT];
        args.extend(local.iter().map(String::as_str));
        let up = parse_builtins(&node(&args)?)?;
        let version = node(&["--version"]).unwrap_or_default();
        section.notes.push(format!("Node {version} lists {} built-in modules; pi-js lists {}.", up.len(), local.len()));
        let unsupported = read_local(root, "crates/pi-js/src/unsupported.rs").unwrap_or_default();
        let count =
            |name: &str| const_array_literals(&unsupported, name, Lang::Rust).map(|items| items.len().to_string());
        let (apis, differences) = (count("UNSUPPORTED_APIS"), count("DIFFERENCES"));
        section.notes.push(format!(
            "pi-js documents {} unsupported APIs (`UNSUPPORTED_APIS`) and {} behavior differences (`DIFFERENCES`).",
            apis.as_deref().unwrap_or("an unknown number of"),
            differences.as_deref().unwrap_or("an unknown number of")
        ));
        (section.missing, section.extra) = diff(&up, &local);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_names_lose_their_prefix() {
        let names = parse_builtins(r#"["fs","fs/promises","node:sea","node:test/reporters"]"#).unwrap();
        assert_eq!(names.into_iter().collect::<Vec<_>>(), ["fs", "fs/promises", "sea", "test/reporters"]);
        assert!(parse_builtins("not json").is_err());
    }
}
