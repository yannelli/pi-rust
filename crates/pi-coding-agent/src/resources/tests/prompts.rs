//! Port of upstream `test/prompt-templates.test.ts`.

use std::path::PathBuf;

use super::write;
use crate::resources::prompts::{
    LoadPromptTemplatesOptions, PromptTemplate, expand_prompt_template, load_prompt_templates, parse_command_args,
    substitute_args,
};
use crate::resources::source_info::create_synthetic_source_info;
use crate::resources::{DiagnosticKind, SourceScope};

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn check(cases: &[(&str, &[&str], &str)]) {
    for (template, values, expected) in cases {
        assert_eq!(substitute_args(template, &args(values)), *expected, "template {template:?} with {values:?}");
    }
}

#[test]
fn substitutes_positional_and_all_arguments() {
    let hundred: Vec<String> = (0..100).map(|index| format!("arg{index}")).collect();
    assert_eq!(substitute_args("$ARGUMENTS", &hundred), hundred.join(" "));
    let fifteen: Vec<String> = (0..15).map(|index| format!("val{index}")).collect();
    assert_eq!(substitute_args("$10 $12 $15", &fifteen), "val9 val11 val14");
    check(&[
        ("Test: $ARGUMENTS", &["a", "b", "c"], "Test: a b c"),
        ("Test: $@", &["a", "b", "c"], "Test: a b c"),
        ("$ARGUMENTS", &["$1", "$ARGUMENTS"], "$1 $ARGUMENTS"),
        ("$@", &["$100", "$1"], "$100 $1"),
        ("$1: $ARGUMENTS", &["prefix", "a", "b"], "prefix: prefix a b"),
        ("$1: $@", &["prefix", "a", "b"], "prefix: prefix a b"),
        ("Test: $ARGUMENTS", &[], "Test: "),
        ("Test: $@", &[], "Test: "),
        ("Test: $1", &[], "Test: "),
        ("$ARGUMENTS and $ARGUMENTS", &["a", "b"], "a b and a b"),
        ("$@ and $ARGUMENTS", &["a", "b"], "a b and a b"),
        ("$1 $2: $ARGUMENTS", &["arg100", "@user"], "arg100 @user: arg100 @user"),
        ("$1 $2 $3 $4 $5", &["a", "b"], "a b   "),
        ("$ARGUMENTS", &["日本語", "🎉", "café"], "日本語 🎉 café"),
        ("$1 $2", &["line1\nline2", "tab\tthere"], "line1\nline2 tab\tthere"),
        ("$1$2", &["a", "b"], "ab"),
        ("$ARGUMENTS", &["first arg", "second arg"], "first arg second arg"),
        ("$0", &["a", "b"], ""),
        ("$1.5", &["a"], "a.5"),
        ("pre$ARGUMENTS", &["a", "b"], "prea b"),
        ("pre$@", &["a", "b"], "prea b"),
        ("$ARGUMENTS", &["a", "", "c"], "a  c"),
        ("$ARGUMENTS", &["  leading  ", "trailing  "], "  leading   trailing  "),
        ("Prefix $ARGUMENTS suffix", &["ARGUMENTS"], "Prefix ARGUMENTS suffix"),
        ("$A $$ $ $ARGS", &["a"], "$A $$ $ $ARGS"),
        ("$arguments $Arguments $ARGUMENTS", &["a", "b"], "$arguments $Arguments a b"),
        ("Price: \\$100", &[], "Price: \\"),
        ("$1: $@ ($ARGUMENTS)", &["first", "second", "third"], "first: first second third (first second third)"),
        ("Just plain text", &["a", "b"], "Just plain text"),
        ("$1 $2 $@", &["a", "b", "c"], "a b a b c"),
        ("$99999999999999999999999", &["a"], ""),
    ]);
}

#[test]
fn substitutes_defaults() {
    check(&[
        ("List exactly ${1:-7} next steps", &[], "List exactly 7 next steps"),
        ("${@:-default}\n${ARGUMENTS:-default}", &[], "default\ndefault"),
        ("${@:-default}\n${ARGUMENTS:-default}", &["This", "would", "be"], "This would be\nThis would be"),
        ("List exactly ${1:-7} next steps", &["3"], "List exactly 3 next steps"),
        ("Mode: ${1:-brief}", &[""], "Mode: brief"),
        ("${1:-7} ${2:-brief}", &[], "7 brief"),
        ("${1:-7} ${2:-brief}", &["3"], "3 brief"),
        ("${1:-7} ${2:-brief}", &["3", "verbose"], "3 verbose"),
        ("${1:-7}", &["$ARGUMENTS"], "$ARGUMENTS"),
        ("${1:-7}", &["$1"], "$1"),
        ("${1:-$ARGUMENTS}", &["a", "b"], "a"),
        ("${3:-$ARGUMENTS}", &["a", "b"], "$ARGUMENTS"),
        ("${1:-seven steps}", &[], "seven steps"),
        ("${3:-fallback}", &["a", "b"], "fallback"),
        ("$1 ${2:-x} $ARGUMENTS", &["a"], "a x a"),
    ]);
}

#[test]
fn substitutes_slices() {
    check(&[
        ("${@:2}", &["a", "b", "c", "d"], "b c d"),
        ("${@:1}", &["a", "b", "c"], "a b c"),
        ("${@:3}", &["a", "b", "c", "d"], "c d"),
        ("${@:2:2}", &["a", "b", "c", "d"], "b c"),
        ("${@:1:1}", &["a", "b", "c"], "a"),
        ("${@:3:1}", &["a", "b", "c", "d"], "c"),
        ("${@:2:3}", &["a", "b", "c", "d", "e"], "b c d"),
        ("${@:99}", &["a", "b"], ""),
        ("${@:10:5}", &["a", "b"], ""),
        ("${@:2:0}", &["a", "b", "c"], ""),
        ("${@:2:99}", &["a", "b", "c"], "b c"),
        ("${@:2} vs $@", &["a", "b", "c"], "b c vs a b c"),
        ("First: ${@:1:1}, All: $@", &["x", "y", "z"], "First: x, All: x y z"),
        ("${@:1}", &["${@:2}", "test"], "${@:2} test"),
        ("${@:2}", &["a", "${@:3}", "c"], "${@:3} c"),
        ("$1: ${@:2}", &["cmd", "arg1", "arg2"], "cmd: arg1 arg2"),
        ("${@:0}", &["a", "b", "c"], "a b c"),
        ("${@:2}", &[], ""),
        ("${@:2}", &["only"], ""),
        ("Process ${@:2} with $1", &["tool", "file1", "file2"], "Process file1 file2 with tool"),
        ("${@:1:2} vs ${@:3:2}", &["a", "b", "c", "d", "e"], "a b vs c d"),
        ("${@:2}", &["cmd", "$100", "@user", "#tag"], "$100 @user #tag"),
        ("prefix${@:2}suffix", &["a", "b", "c"], "prefixb csuffix"),
        (
            "Run $1 on ${@:2:2}, then process $@",
            &["eslint", "file1.ts", "file2.ts", "file3.ts"],
            "Run eslint on file1.ts file2.ts, then process eslint file1.ts file2.ts file3.ts",
        ),
    ]);
}

#[test]
fn parses_shell_like_arguments() {
    let cases: &[(&str, &[&str])] = &[
        ("a b c", &["a", "b", "c"]),
        ("\"first arg\" second", &["first arg", "second"]),
        ("'first arg' second", &["first arg", "second"]),
        ("\"double\" 'single' \"double again\"", &["double", "single", "double again"]),
        ("", &[]),
        ("a  b   c", &["a", "b", "c"]),
        ("a\tb\tc", &["a", "b", "c"]),
        ("\"\" \" \"", &["", " "]),
        ("a \"\" c ''", &["a", "", "c", ""]),
        ("$100 @user #tag", &["$100", "@user", "#tag"]),
        ("日本語 🎉 café", &["日本語", "🎉", "café"]),
        ("\"line1\nline2\" second", &["line1\nline2", "second"]),
        ("label-2\n\nHere is some description #2.", &["label-2", "Here", "is", "some", "description", "#2."]),
        ("a\n\n\tb  c", &["a", "b", "c"]),
        ("\"quoted \\\"text\\\"\"", &["quoted \\text\\"]),
        ("a b c   ", &["a", "b", "c"]),
        ("   a b c", &["a", "b", "c"]),
    ];
    for (input, expected) in cases {
        assert_eq!(parse_command_args(input), args(expected), "input {input:?}");
    }
    let parsed = parse_command_args("Button \"onClick handler\" \"disabled support\"");
    assert_eq!(
        substitute_args("Create a React component named $1 with features: $ARGUMENTS", &parsed),
        "Create a React component named Button with features: Button onClick handler disabled support"
    );
}

fn template(name: &str, content: &str) -> PromptTemplate {
    PromptTemplate {
        name: name.into(),
        description: "test".into(),
        argument_hint: None,
        content: content.into(),
        source_info: create_synthetic_source_info("/tmp/arg-test.md", "local", None, None),
        file_path: PathBuf::from("/tmp/arg-test.md"),
    }
}

#[test]
fn expands_templates() {
    let templates = [template("arg-test", "- arg1: $1\n- rest: ${@:2}"), template("one", "arg1: $1")];
    assert_eq!(
        expand_prompt_template("/arg-test label-2\n\nHere is some description #2.", &templates),
        "- arg1: label-2\n- rest: Here is some description #2."
    );
    assert_eq!(expand_prompt_template("/one\nlabel-2", &templates), "arg1: label-2");
    assert_eq!(expand_prompt_template("/one", &templates), "arg1: ");
    assert_eq!(expand_prompt_template("/missing x", &templates), "/missing x");
    assert_eq!(expand_prompt_template("plain", &templates), "plain");
    assert_eq!(crate::resources::expand_user_input("/one x", &[], &templates).unwrap(), "arg1: x");
}

#[test]
fn loads_argument_hints_descriptions_and_diagnostics() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("prompts");
    write(&dir.join("pr.md"), "---\ndescription: Review PRs from URLs\nargument-hint: \"<PR-URL>\"\n---\nGiven: $@");
    write(&dir.join("wr.md"), "---\ndescription: Wrap it\nargument-hint: \"[instructions]\"\n---\nWrap: $ARGUMENTS");
    write(&dir.join("empty-hint.md"), "---\ndescription: Empty hint\nargument-hint: \"\"\n---\nDo something");
    write(&dir.join("first-line.md"), &format!("\n\n{}\nsecond", "x".repeat(70)));
    write(&dir.join("invalid.md"), "---\ndescription: Broken: unquoted colon\n---\nDo something.\n");
    write(&dir.join("nested/skip.md"), "Nested files are not templates of a directory");
    write(&dir.join("notes.txt"), "Not markdown");
    let result = load_prompt_templates(&LoadPromptTemplatesOptions {
        cwd: temp.path().into(),
        agent_dir: temp.path().join("agent"),
        prompt_paths: vec![dir.display().to_string()],
        include_defaults: false,
    });
    let find = |name: &str| result.templates.iter().find(|template| template.name == name).unwrap();
    assert_eq!(find("pr").argument_hint.as_deref(), Some("<PR-URL>"));
    assert_eq!(find("pr").description, "Review PRs from URLs");
    assert_eq!(find("pr").content, "Given: $@");
    assert_eq!(find("wr").argument_hint.as_deref(), Some("[instructions]"));
    assert_eq!(find("empty-hint").argument_hint, None);
    assert_eq!(find("first-line").description, format!("{}...", "x".repeat(60)));
    assert_eq!(find("pr").source_info.scope, SourceScope::Temporary);
    let names: Vec<&str> = result.templates.iter().map(|template| template.name.as_str()).collect();
    assert_eq!(names, ["empty-hint", "first-line", "pr", "wr"]);
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].kind, DiagnosticKind::Warning);
    assert!(result.diagnostics[0].path_is(&dir.join("invalid.md")));
    assert!(result.diagnostics[0].message.contains("line 1, column "), "{}", result.diagnostics[0].message);
}
