//! Ported from upstream `packages/codemode/test/source.test.ts`.

use pi_codemode::{CodemodeSourceOptions, ParsedCodemodeSource, parse_codemode_source};

fn parsed(code: &str, options: CodemodeSourceOptions) -> ParsedCodemodeSource {
    ParsedCodemodeSource { code: code.into(), options }
}

#[test]
fn returns_plain_code_unchanged() {
    assert_eq!(parse_codemode_source("text('hi')").unwrap(), parsed("text('hi')", Default::default()));
    assert_eq!(
        parse_codemode_source("// just a comment\nreturn 1").unwrap(),
        parsed("// just a comment\nreturn 1", Default::default())
    );
}

#[test]
fn parses_the_options_line_and_keeps_line_numbers() {
    assert_eq!(
        parse_codemode_source("// @options: {\"timeout_ms\": 10}\nconst a = 1;\ntext(a)").unwrap(),
        parsed("\nconst a = 1;\ntext(a)", CodemodeSourceOptions { timeout_ms: Some(10), max_output_tokens: None })
    );
    assert_eq!(
        parse_codemode_source("  // @options:{\"max_output_tokens\":0,\"timeout_ms\":1500}\r\ntext(1)")
            .unwrap()
            .options,
        CodemodeSourceOptions { max_output_tokens: Some(0), timeout_ms: Some(1500) }
    );
    assert_eq!(parse_codemode_source("// @options: {}\ntext(1)").unwrap(), parsed("\ntext(1)", Default::default()));
    assert_eq!(
        parse_codemode_source("// @options: {\"max_output_tokens\": 2.0}\ntext(1)").unwrap().options.max_output_tokens,
        Some(2)
    );
}

#[test]
fn only_treats_the_first_line_as_an_options_line() {
    assert_eq!(parse_codemode_source("// @optionsx {}\ntext(1)").unwrap().options, Default::default());
    assert_eq!(parse_codemode_source("text(\"// @options: {}\")").unwrap().options, Default::default());
}

// #10631: an options line after the first line used to be ignored, so its timeout never applied.
#[test]
fn rejects_an_options_line_that_is_not_the_first_line() {
    let cases = [
        ("// count things\n// @options: {\"timeout_ms\": 1000}\nwhile (true) {}", 2),
        ("\n  // @options: {\"timeout_ms\": 1000}\nwhile (true) {}", 2),
        ("text(1)\r\n// @options: {\"timeout_ms\": 1}", 2),
        ("// @options: {\"timeout_ms\": 1}\ntext(1)\n// @options: {\"timeout_ms\": 2}", 3),
    ];
    for (input, line) in cases {
        let error = parse_codemode_source(input).unwrap_err();
        let expected = format!("The // @options: line must be the first line of the input; found one on line {line}");
        assert_eq!(error.to_string(), expected, "{input:?}");
    }
}

#[test]
fn rejects_empty_input_and_invalid_options() {
    let cases = [
        ("", "Expected JavaScript source text (non-empty)"),
        ("  \n", "Expected JavaScript source text (non-empty)"),
        ("// @options:\ntext(1)", "@options must be a JSON object with supported fields"),
        ("// @options: {timeout_ms: 1}\ntext(1)", "@options must be valid JSON with supported fields"),
        ("// @options: [1]\ntext(1)", "@options must be a JSON object with supported fields"),
        (
            "// @options: {\"yield\": 1}\ntext(1)",
            "@options only supports `max_output_tokens` and `timeout_ms`; got `yield`",
        ),
        (
            "// @options: {\"max_output_tokens\": 1.5}\ntext(1)",
            "@options field `max_output_tokens` must be a non-negative safe integer",
        ),
        ("// @options: {\"max_output_tokens\": -1}\ntext(1)", "must be a non-negative safe integer"),
        ("// @options: {\"timeout_ms\": 0}\ntext(1)", "@options field `timeout_ms` must be a positive integer"),
        ("// @options: {\"timeout_ms\": 2147483648}\ntext(1)", "must be a positive integer up to 2147483647"),
        (
            "// @options: {\"timeout_ms\": 1}",
            "The @options line must be followed by JavaScript source on subsequent lines",
        ),
        (
            "// @options: {\"timeout_ms\": 1}\n  \n",
            "The @options line must be followed by JavaScript source on subsequent lines",
        ),
    ];
    for (input, message) in cases {
        let error = parse_codemode_source(input).unwrap_err();
        assert!(error.to_string().contains(message), "{input:?}: {error}");
    }
}
