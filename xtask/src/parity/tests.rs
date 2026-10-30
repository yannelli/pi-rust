//! The whole report on a tiny fake upstream tree.

use std::path::Path;

use super::report::{Allowlist, Status};
use super::*;

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn fake_upstream() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let up = dir.path();
    let agent = "packages/coding-agent/src";
    write(up, &format!("{agent}/cli/args.ts"), "if (arg === \"--model\") {} else if (arg === \"--zzz-fake\") {}");
    write(
        up,
        &format!("{agent}/core/settings-schema.ts"),
        "const TerminalSettingsSchema = Type.Object({\n\tshowImages: Type.Optional(Type.Boolean()),\n\tzzzTerm: Type.Optional(Type.Boolean()),\n});\n\
         export const SettingsSchema = Type.Object({\n\ttheme: Type.Optional(Type.String()),\n\tzzzFake: Type.Optional(Type.String()),\n});",
    );
    write(
        up,
        &format!("{agent}/core/slash-commands.ts"),
        "export const BUILTIN_SLASH_COMMANDS = [\n\t{ name: \"model\", description: \"m\" },\n\t{ name: \"zzzfake\", description: \"z\" },\n];",
    );
    write(up, &format!("{agent}/core/tools/bash.ts"), "const t = { name: \"bash\" };");
    write(up, &format!("{agent}/core/tools/zzztool.ts"), "const t = { name: \"zzztool\" };");
    write(up, &format!("{agent}/core/tools/truncate.ts"), "export const MAX = 1;");
    write(
        up,
        &format!("{agent}/core/extensions/types.ts"),
        "export interface ExtensionAPI {\n\tregisterTool(t: T): void;\n\tzzzFakeMethod(): void;\n}",
    );
    write(
        up,
        "packages/ai/src/models.generated.ts",
        "export const MODELS: {\n\treadonly \"anthropic\": typeof A;\n\treadonly \"zzzprov\": typeof Z;\n} = {};",
    );
    write(
        up,
        "packages/ai/src/providers/data/anthropic.json",
        r#"{"anthropic-messages": {"chat:claude-zzz-fake": {}}}"#,
    );
    write(
        up,
        &format!("{agent}/core/extensions/virtual-modules.ts"),
        "import * as tui from \"@earendil-works/pi-tui\";\nexport const VIRTUAL_MODULES = {\n\t\"@earendil-works/pi-tui\": tui,\n\t\"@zzz/alias\": tui,\n};",
    );
    write(up, "packages/tui/package.json", r#"{"name": "@earendil-works/pi-tui", "main": "dist/index.js"}"#);
    write(up, "packages/tui/src/text.ts", "export class Text {}\nexport type Zzz = 1;");
    write(
        up,
        "packages/tui/src/index.ts",
        "export { Text, type Zzz } from \"./text.js\";\nexport const zzzTuiExport = 1;",
    );
    write(up, "packages/zzzpkg/package.json", "{}");
    write(up, "packages/ai/package.json", "{}");
    dir
}

fn synced() -> Synced {
    Synced {
        repository: "earendil-works/pi".into(),
        branch: "main".into(),
        commit: "d2e7d8b7c1d31d3661f769d5caef7ba8d50c3e03".into(),
        version: "1.3.0".into(),
        date: "2026-10-19".into(),
    }
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

#[test]
fn report_on_a_fake_upstream_tree() {
    let dir = fake_upstream();
    let report = build_report(dir.path(), root(), &synced(), Allowlist::default());
    let text = report.render();
    assert_eq!(report.sections.len(), 10);
    for heading in
        ["Upstream commits", "CLI flags", "Settings keys", "Slash commands", "Built-in tools", "Model catalog"]
    {
        assert!(text.contains(heading), "{heading}");
    }
    for heading in ["Extension API", "Node built-ins", "Packages", "Last sync: `d2e7d8b7c`"] {
        assert!(text.contains(heading), "{heading}");
    }
    assert!(text.contains("Upstream HEAD: unknown"));
    let find = |id: &str| report.sections.iter().find(|section| section.id == id).unwrap();
    let names = |items: &[report::Item]| items.iter().map(|item| item.name.clone()).collect::<Vec<_>>();
    assert!(matches!(&find("commits").status, Status::Skipped(reason) if reason.contains("git checkout")));
    assert_eq!(names(&find("cliFlags").missing), ["--zzz-fake"]);
    assert!(names(&find("cliFlags").extra).contains(&"--provider".to_string()));
    assert_eq!(names(&find("settings").missing), ["terminal.zzzTerm", "zzzFake"]);
    assert!(names(&find("settings").extra).contains(&"defaultModel".to_string()));
    assert!(names(&find("settings").extra).contains(&"terminal.notifyOnFinish".to_string()));
    assert!(!names(&find("settings").extra).contains(&"theme".to_string()));
    assert!(!names(&find("settings").extra).contains(&"terminal.showImages".to_string()));
    assert_eq!(names(&find("slashCommands").missing), ["zzzfake"]);
    assert!(names(&find("slashCommands").extra).contains(&"quit".to_string()));
    assert_eq!(names(&find("tools").missing), ["zzztool"]);
    assert!(names(&find("tools").extra).contains(&"grep".to_string()));
    assert!(!names(&find("tools").missing).contains(&"truncate".to_string()));
    assert_eq!(names(&find("models").missing), ["anthropic", "zzzprov"]);
    assert_eq!(names(&find("extensionApi").missing), ["zzzFakeMethod"]);
    assert_eq!(names(&find("packages").missing), ["zzzpkg"]);
    let exports = names(&find("moduleExports").missing);
    assert_eq!(exports, ["@zzz/alias: (module)", "@earendil-works/pi-tui: zzzTuiExport"]);
    assert!(names(&find("packages").extra).contains(&"pi-js".to_string()));
    assert!(report.findings() > 0);
}

#[test]
fn allowlisted_findings_are_counted_and_listed() {
    let dir = fake_upstream();
    let allowlist =
        Allowlist::parse(r#"{"cliFlags": [{"item": "--zzz-fake", "reason": "kept out on purpose"}]}"#).unwrap();
    let report = build_report(dir.path(), root(), &synced(), allowlist);
    let flags = report.sections.iter().find(|section| section.id == "cliFlags").unwrap();
    assert!(flags.missing.is_empty());
    assert!(report.render().contains("- `--zzz-fake`: kept out on purpose"));
}

#[test]
fn arguments_are_validated() {
    let strings = |items: &[&str]| items.iter().map(|item| item.to_string()).collect::<Vec<_>>();
    let options = parse_args(&strings(&["--upstream", "/x", "--out", "r.md", "--strict"])).unwrap().unwrap();
    assert!(
        (options.upstream.to_str(), options.out.as_deref(), options.strict)
            == (Some("/x"), Some(Path::new("r.md")), true)
    );
    assert!(parse_args(&strings(&["--out", "r.md"])).is_err());
    assert!(parse_args(&strings(&["--upstream"])).is_err());
    assert!(parse_args(&strings(&["--upstream", "/x", "--bogus"])).is_err());
    assert!(parse_args(&strings(&["--help"])).unwrap().is_none());
}

#[test]
fn run_writes_the_report_and_strict_fails_on_skipped_sections() {
    let dir = fake_upstream();
    let out = dir.path().join("out/report.md");
    let args = |extra: &[&str]| {
        let mut args =
            vec!["--upstream".to_string(), dir.path().display().to_string(), "--out".into(), out.display().to_string()];
        args.extend(extra.iter().map(|item| item.to_string()));
        args
    };
    run(&args(&[]), root()).unwrap();
    assert!(std::fs::read_to_string(&out).unwrap().starts_with("# Parity report"));
    // The fixture is not a git repository, so the commits section cannot run.
    assert!(run(&args(&["--strict"]), root()).unwrap_err().contains("did not run: commits"));
    assert!(run(&["--upstream".into(), "/definitely/missing".into()], root()).is_err());
}
