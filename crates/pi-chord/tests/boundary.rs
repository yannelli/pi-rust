//! Port of upstream `test/boundary.test.ts`: Chord stands alone apart from the optional `pi-js` and `pi-telemetry`,
//! a leaf crate that `Context::telemetry` needs.

use std::path::{Path, PathBuf};

const CRATE: &str = env!("CARGO_MANIFEST_DIR");

fn sources(directory: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn does_not_depend_on_pi_crates_other_than_pi_telemetry_or_files_outside_chord() {
    let manifest = std::fs::read_to_string(Path::new(CRATE).join("Cargo.toml")).unwrap();
    let dependencies: Vec<&str> = manifest
        .lines()
        .skip_while(|line| line.trim() != "[dependencies]")
        .skip(1)
        .take_while(|line| !line.starts_with('['))
        .filter_map(|line| line.split_once('=').map(|(name, _)| name.trim().trim_end_matches(".workspace")))
        .filter(|name| name.starts_with("pi-"))
        .collect();
    assert_eq!(dependencies, ["pi-js", "pi-telemetry"]);
    assert!(manifest.contains("pi-js = { path = \"../pi-js\", optional = true }"));

    let mut files = Vec::new();
    sources(&Path::new(CRATE).join("src"), &mut files);
    let mut violations = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let shown = file.strip_prefix(CRATE).unwrap().display().to_string();
        for (number, line) in text.lines().enumerate() {
            let crate_use =
                line.contains("pi_") && (line.contains("use pi_") || line.contains(" pi_") || line.contains("(pi_"));
            let foreign = crate_use
                && line
                    .split(|char: char| !char.is_alphanumeric() && char != '_')
                    .any(|word| word.starts_with("pi_") && !["pi_js", "pi_chord", "pi_telemetry"].contains(&word));
            let outside = line.contains("include_str!(\"..") || line.contains("#[path");
            if foreign || outside {
                violations.push(format!("{shown}:{}: {}", number + 1, line.trim()));
            }
        }
    }
    assert!(violations.is_empty(), "{violations:#?}");
}
