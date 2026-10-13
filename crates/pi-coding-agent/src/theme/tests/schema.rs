//! Theme validation messages, matched against upstream TypeBox output, and color resolution errors.

use serde_json::{Value, json};

use super::*;

fn error_for(edit: impl FnOnce(&mut Value)) -> String {
    let mut theme = builtin_json("dark");
    edit(&mut theme);
    validate_theme_json("t", &theme).expect_err("invalid theme").0
}

fn other_errors(lines: &[&str]) -> String {
    format!(
        "Invalid theme \"t\":\n\n\nOther errors:\n{}",
        lines.iter().map(|line| format!("  - {line}")).collect::<Vec<_>>().join("\n")
    )
}

#[test]
fn rejects_unknown_properties_at_every_level() {
    let top = error_for(|theme| theme["unsupported"] = json!(true));
    assert_eq!(top, other_errors(&["/unsupported: schema is false", "/: must not have additional properties"]));
    let color = error_for(|theme| theme["colors"]["custom"] = json!("#fff"));
    assert_eq!(
        color,
        other_errors(&["/colors/custom: schema is false", "/colors: must not have additional properties"])
    );
    let export = error_for(|theme| theme["export"] = json!({ "pageBg": "#000", "other": "#111" }));
    assert_eq!(
        export,
        other_errors(&["/export/other: schema is false", "/export: must not have additional properties"])
    );
}

#[test]
fn lists_missing_color_tokens_sorted() {
    let message = error_for(|theme| {
        let colors = theme["colors"].as_object_mut().expect("colors");
        for token in ["accent", "bashMode", "text", "scrollbarTrack"] {
            colors.remove(token);
        }
    });
    let expected = "Invalid theme \"t\":\n\nMissing required color tokens:\n  - accent\n  - bashMode\n  - text\n\n\
        Please add these colors to your theme's \"colors\" object.\n\
        See the built-in themes (dark.json, light.json) for reference values.";
    assert_eq!(message, expected);
}

#[test]
fn reports_value_types_and_ranges_like_typebox() {
    let invalid = |path: &str, middle: &str| {
        other_errors(&[
            &format!("{path}: must be string"),
            &format!("{path}: {middle}"),
            &format!("{path}: must match a schema in anyOf"),
        ])
    };
    assert_eq!(error_for(|t| t["colors"]["accent"] = json!(true)), invalid("/colors/accent", "must be integer"));
    assert_eq!(error_for(|t| t["colors"]["accent"] = json!(300)), invalid("/colors/accent", "must be <= 255"));
    assert_eq!(error_for(|t| t["colors"]["accent"] = json!(1.5)), invalid("/colors/accent", "must be integer"));
    assert_eq!(error_for(|t| t["colors"]["accent"] = json!(-1)), invalid("/colors/accent", "must be >= 0"));
    assert_eq!(error_for(|t| t["vars"] = json!({ "a": null })), invalid("/vars/a", "must be integer"));
    assert_eq!(error_for(|t| t["name"] = json!(5)), other_errors(&["/name: must be string"]));
    assert_eq!(error_for(|t| t["name"] = json!("")), other_errors(&["/name: must match pattern \"^[^/]+$\""]));
    assert_eq!(error_for(|t| t["$schema"] = json!(3)), other_errors(&["/$schema: must be string"]));
    let appearance = error_for(|t| t["appearance"] = json!("dim"));
    let constant = "/appearance: must be equal to constant";
    assert_eq!(appearance, other_errors(&[constant, constant, "/appearance: must match a schema in anyOf"]));
}

#[test]
fn reports_document_shape_errors_and_caps_at_eight() {
    let validate = |value: Value| validate_theme_json("t", &value).expect_err("invalid").0;
    assert_eq!(validate(json!([1, 2])), other_errors(&["/: must be object"]));
    assert_eq!(validate(Value::Null), other_errors(&["/: must be object"]));
    assert_eq!(validate(json!({})), other_errors(&["/: must have required properties name, colors"]));
    assert_eq!(validate(json!({ "name": "x", "colors": [] })), other_errors(&["/colors: must be object"]));
    let many = error_for(|theme| {
        theme["unsupported"] = json!(1);
        theme["other"] = json!(2);
        theme["colors"]["accent"] = json!(false);
        theme["colors"]["foo"] = json!(1);
    });
    let expected = other_errors(&[
        "/unsupported: schema is false",
        "/other: schema is false",
        "/: must not have additional properties",
        "/colors/foo: schema is false",
        "/colors: must not have additional properties",
        "/colors/accent: must be string",
        "/colors/accent: must be integer",
        "/colors/accent: must match a schema in anyOf",
    ]);
    assert_eq!(many, expected);
}

#[test]
fn rejects_slashes_in_names_before_schema_checks() {
    let message = error_for(|theme| theme["name"] = json!("a/b"));
    assert_eq!(
        message,
        "Invalid theme name \"a/b\": theme names cannot contain \"/\" because it is reserved for automatic light/dark theme settings."
    );
}

#[test]
fn reports_unreadable_json_and_bad_color_values() {
    let parse = parse_theme_json_content("broken.json", "{ nope").expect_err("bad json").0;
    assert!(parse.starts_with("Failed to parse theme broken.json: "), "{parse}");
    let resolve = |edit: fn(&mut Value)| {
        let mut theme = builtin_json("dark");
        edit(&mut theme);
        let json = validate_theme_json("t", &theme).expect("schema-valid");
        create_theme(&json, None, None).expect_err("invalid colors").0
    };
    assert_eq!(resolve(|t| t["colors"]["accent"] = json!("nope")), "Variable reference not found: nope");
    assert_eq!(resolve(|t| t["colors"]["accent"] = json!("#12")), "Invalid color value: #12");
    let circular = resolve(|t| {
        t["vars"]["a"] = json!("b");
        t["vars"]["b"] = json!("a");
        t["colors"]["accent"] = json!("a");
    });
    assert_eq!(circular, "Circular variable reference detected: a");
}

#[test]
fn resource_themes_report_invalid_files_as_warnings() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut valid = builtin_json("dark");
    valid["name"] = json!("capability-test");
    valid["colors"]["userMessageBg"] = json!("#3c3544");
    let valid_path = write_theme(dir.path(), &valid);
    let mut invalid = valid.clone();
    invalid["name"] = json!("broken");
    invalid["unsupported"] = json!(true);
    let invalid_path = write_theme(dir.path(), &invalid);
    let file = |path: &std::path::Path| crate::resources::ThemeFile {
        path: path.to_path_buf(),
        name: None,
        source_info: crate::resources::SourceInfo {
            path: path.display().to_string(),
            source: "local".into(),
            scope: Default::default(),
            origin: Default::default(),
            base_dir: None,
        },
    };
    let truecolor = Some(pi_tui::colors::TerminalColorMode::Truecolor);
    let loaded = load_theme_files(&[file(&valid_path), file(&invalid_path)], truecolor);
    assert_eq!(loaded.themes.len(), 1);
    assert_eq!(loaded.themes[0].bg(ThemeBg::UserMessageBg, "x"), "\x1b[48;2;60;53;68mx\x1b[49m");
    assert_eq!(loaded.themes[0].source_info().map(|info| info.source.as_str()), Some("local"));
    assert_eq!(loaded.diagnostics.len(), 1);
    assert_eq!(loaded.diagnostics[0].path.as_deref(), Some(invalid_path.display().to_string().as_str()));
    assert!(loaded.diagnostics[0].message.contains("/unsupported"));
    let color256 = load_theme_files(&[file(&valid_path)], Some(pi_tui::colors::TerminalColorMode::Color256));
    assert_eq!(color256.themes[0].bg(ThemeBg::UserMessageBg, "x"), "\x1b[48;5;59mx\x1b[49m");
}

#[test]
fn published_schema_lists_every_token() {
    let schema = theme_json_schema();
    let properties = schema["properties"]["colors"]["properties"].as_object().expect("properties");
    assert_eq!(properties.len(), TOKEN_COUNT);
    let required = schema["properties"]["colors"]["required"].as_array().expect("required");
    assert_eq!(required.len(), TOKEN_COUNT - 5);
}
