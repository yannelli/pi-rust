//! Markdown transformer tests (upstream `mermaid.test.ts` and the transform chain).

use std::sync::Arc;

use super::mermaid::code_blocks_for_test;
use super::*;
use crate::settings::schema::MermaidRenderingMode;
use crate::theme::ThemeColor;

struct Options {
    max_width: usize,
    is_streaming: bool,
    message_type: MarkdownMessageType,
    mode: MermaidRenderingMode,
    theme: Option<Arc<dyn MermaidStyle + Send + Sync>>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_width: 100,
            is_streaming: false,
            message_type: MarkdownMessageType::Assistant,
            mode: MermaidRenderingMode::Streaming,
            theme: None,
        }
    }
}

fn transform(markdown: &str, options: Options) -> String {
    let mode = options.mode;
    let transformer = create_mermaid_markdown_transformer(MermaidTransformerOptions {
        get_mode: Arc::new(move || mode),
        theme: options.theme,
    });
    let context = MarkdownTransformContext {
        message_type: options.message_type,
        is_streaming: options.is_streaming,
        available_width: options.max_width,
        message_id: None,
        timestamp: None,
    };
    transformer(markdown, &context).expect("transformed")
}

/// Wraps text in tags naming the token, like upstream's fake theme.
struct TagStyle;

impl MermaidStyle for TagStyle {
    fn fg(&self, color: ThemeColor, text: &str) -> String {
        format!("<{color}>{text}</{color}>")
    }

    fn bold(&self, text: &str) -> String {
        format!("<bold>{text}</bold>")
    }
}

#[test]
fn replaces_mermaid_code_blocks_with_unicode_diagrams() {
    let markdown = "Before\n\n```mermaid\nflowchart LR\n  A[Start] --> B[Done]\n```\nAfter";
    let rendered = transform(markdown, Options::default());
    assert!(rendered.contains("Before"));
    assert!(rendered.contains("┌───────┐"));
    assert!(rendered.contains("│ Start ├───▶│ Done │"));
    assert!(rendered.contains("└───────┘    └──────┘`\nAfter"), "{rendered}");
    assert!(!rendered.contains("```mermaid"));
}

#[test]
fn leaves_unsupported_and_oversized_diagrams_unchanged() {
    let unsupported = "```mermaid\npie\n  title Pets\n  \"Dogs\" : 4\n```";
    let oversized = "```mermaid\nflowchart LR\n  A[Start] --> B[Done]\n```";
    assert_eq!(transform(unsupported, Options::default()), unsupported);
    assert_eq!(transform(oversized, Options { max_width: 10, ..Options::default() }), oversized);
}

#[test]
fn maps_semantic_spans_through_the_theme() {
    let rendered = transform(
        "```mermaid\nflowchart LR\n  A --> B\n```",
        Options { theme: Some(Arc::new(TagStyle)), ..Options::default() },
    );
    assert!(rendered.contains("<borderMuted>"), "{rendered}");
    assert!(rendered.contains("<accent>"), "{rendered}");
}

#[test]
fn renders_incomplete_blocks_while_streaming() {
    let rendered =
        transform("```mermaid\nflowchart LR\n  A --> B", Options { is_streaming: true, ..Options::default() });
    assert!(rendered.contains("───▶"), "{rendered}");
}

#[test]
fn renders_diagrams_with_class_assignments() {
    let rendered = transform("```mermaid\nflowchart LR\n  A[Foo]:::highlight --> B[Bar]\n```", Options::default());
    assert!(!rendered.contains("```mermaid"));
    assert!(!rendered.contains("Mermaid diagram not rendered"));
    assert!(rendered.contains("│ Foo ├───▶│ Bar │"), "{rendered}");
}

#[test]
fn falls_back_to_the_code_block_with_a_warning_after_streaming() {
    let markdown = "```mermaid\nflowchart LR\n  A[Foo] invalid\n```";
    let final_render = transform(markdown, Options::default());
    let followed = transform(&format!("{markdown}\nFollowing text"), Options::default());
    let streaming = transform(markdown, Options { is_streaming: true, ..Options::default() });
    assert!(final_render.contains(markdown));
    assert!(final_render.contains("```\n`Mermaid diagram not rendered"), "{final_render}");
    assert!(final_render.contains("dropped, expected a link: \"invalid\""), "{final_render}");
    assert!(!final_render.contains("more)"));
    assert!(followed.contains("  \nFollowing text"), "{followed}");
    assert!(!streaming.contains("Mermaid diagram not rendered"));
    assert!(!streaming.contains("```mermaid"));
    assert!(streaming.contains("│ Foo │"), "{streaming}");
}

#[test]
fn summarizes_additional_partial_render_warnings() {
    let markdown = "```mermaid\nflowchart LR\n  A[Foo] invalid\n  B[Bar] also-invalid\n```";
    let rendered = transform(markdown, Options::default());
    assert!(rendered.contains(markdown));
    assert!(rendered.contains("dropped, expected a link: \"invalid\""));
    assert!(rendered.contains("(+1 more)"));
    assert!(!rendered.contains("dropped, expected a link: \"also-invalid\""));
}

#[test]
fn respects_rendering_modes_and_skips_thinking_blocks() {
    let markdown = "```mermaid\nflowchart LR\n  A --> B\n```";
    assert_eq!(transform(markdown, Options { mode: MermaidRenderingMode::Off, ..Options::default() }), markdown);
    let final_streaming = Options { mode: MermaidRenderingMode::Final, is_streaming: true, ..Options::default() };
    assert_eq!(transform(markdown, final_streaming), markdown);
    assert!(
        !transform(markdown, Options { mode: MermaidRenderingMode::Final, ..Options::default() })
            .contains("```mermaid")
    );
    let thinking = Options { message_type: MarkdownMessageType::AssistantThinking, ..Options::default() };
    assert_eq!(transform(markdown, thinking), markdown);
}

#[test]
fn finds_top_level_code_blocks_with_marked_raw_extents() {
    let blocks = |markdown: &str| code_blocks_for_test(markdown);
    let fence = "```mermaid\nflowchart LR\n  A --> B\n```";
    assert_eq!(blocks(&format!("Before\n\n{fence}\nAfter"))[0].0, format!("{fence}\n"));
    assert_eq!(blocks(&format!("{fence}\n\n\nX"))[0].0, fence);
    assert_eq!(blocks("```mermaid\nA")[0], ("```mermaid\nA".into(), "mermaid".into(), "A".into()));
    assert!(blocks("- item\n  ```mermaid\n  A\n  ```\n").is_empty());
    assert!(blocks("> ```mermaid\n> A\n> ```").is_empty());
    assert_eq!(blocks("    ```mermaid\n    A\n    ```")[0].1, "", "indented code has no info string");
    assert_eq!(
        blocks("~~~ Mermaid extra\nA\n~~~\n")[0],
        ("~~~ Mermaid extra\nA\n~~~\n".into(), "Mermaid extra".into(), "A".into())
    );
    assert_eq!(blocks("text\n```mermaid\nA\n```\nmore")[0].0, "```mermaid\nA\n```\n");
}

#[test]
fn keeps_non_mermaid_source_byte_for_byte() {
    let markdown = "a\r\n\r\n```js\r\nx\r\n```\r\n\r\n\r\n[a]: x\n\n[a]: y\n\t- item\r\n";
    assert_eq!(transform(markdown, Options::default()), markdown);
    let mermaid_word_only = "The word mermaid\r\n- ```mermaid\r\n  A --> B\r\n  ```\r\n";
    assert_eq!(transform(mermaid_word_only, Options::default()), mermaid_word_only);
}

#[test]
fn replaces_blocks_in_crlf_source_and_keeps_the_rest() {
    let diagram = "```mermaid\r\nflowchart LR\r\n  A --> B\r\n```";
    let markdown = format!("[a]: x\r\n\r\n[a]: y\r\n\r\nBefore\r\n\r\n{diagram}\r\nAfter\r\n");
    let rendered = transform(&markdown, Options::default());
    assert!(rendered.starts_with("[a]: x\r\n\r\n[a]: y\r\n\r\nBefore\r\n\r\n`"), "{rendered:?}");
    assert!(rendered.ends_with("`\nAfter\r\n"), "{rendered:?}");
    assert!(rendered.contains("───▶"), "{rendered:?}");
    let oversized = transform(&markdown, Options { max_width: 5, ..Options::default() });
    assert_eq!(oversized, markdown);
}

#[test]
fn renders_mermaid_between_latex_delimiters_like_plain_marked() {
    let rendered = transform("$$\n```mermaid\nflowchart LR\n  A --> B\n```\n$$", Options::default());
    assert!(rendered.starts_with("$$\n`"), "{rendered:?}");
    assert!(rendered.ends_with("`\n$$"), "{rendered:?}");
}

#[test]
fn code_spans_choose_fences_longer_than_inner_backticks() {
    let transformer = create_markdown_transform(
        MarkdownMessageType::Assistant,
        false,
        vec![create_mermaid_markdown_transformer(MermaidTransformerOptions {
            get_mode: Arc::new(|| MermaidRenderingMode::Streaming),
            theme: None,
        })],
        None,
    );
    let rendered = transformer("```mermaid\nflowchart LR\n  A[plain ` tick] --> B[two `` ticks]\n```", 100);
    assert!(rendered.contains("```│ plain ` tick ├───▶│ two `` ticks │```"), "{rendered}");
}

#[test]
fn transformers_run_in_order_and_failures_keep_the_markdown() {
    let upper: MarkdownTransformer = Arc::new(|markdown, _| Some(markdown.to_uppercase()));
    let skip: MarkdownTransformer = Arc::new(|_, _| None);
    let failing: MarkdownTransformer = Arc::new(|_, _| panic!("transformer failed"));
    let width: MarkdownTransformer = Arc::new(|markdown, context| {
        let streaming = if context.is_streaming { "streaming" } else { "final" };
        Some(format!("{markdown} {} {streaming} {:?}", context.available_width, context.message_type))
    });
    let transform = create_markdown_transform(MarkdownMessageType::User, true, vec![upper, skip, failing, width], None);
    assert_eq!(transform("hi", 42), "HI 42 streaming User");
}

/// Tags spans with a prefix the test can change between calls.
struct SwitchStyle(std::sync::Mutex<&'static str>);

impl MermaidStyle for SwitchStyle {
    fn fg(&self, color: ThemeColor, text: &str) -> String {
        format!("<{}{color}>{text}", self.0.lock().expect("style"))
    }

    fn bold(&self, text: &str) -> String {
        format!("<bold>{text}")
    }
}

#[test]
fn a_reused_transformer_matches_a_fresh_one_for_streamed_and_changed_context() {
    let diagram = "```mermaid\nflowchart LR\n  A[Start] --> B[Done]\n```";
    let documents = [
        format!(
            "{}{diagram}\n\nAfter\n\n```mermaid\nflowchart LR\n  A[Foo] invalid\n```\nTail",
            "Prose line.\n\n".repeat(8)
        ),
        format!("# Plan\r\n\r\n{}\r\nText\r\n```js\r\nx\r\n```", diagram.replace('\n', "\r\n")),
    ];
    let mode = Arc::new(std::sync::Mutex::new(MermaidRenderingMode::Streaming));
    let style = Arc::new(SwitchStyle(std::sync::Mutex::new("a")));
    let mode_source = mode.clone();
    let reused = create_mermaid_markdown_transformer(MermaidTransformerOptions {
        get_mode: Arc::new(move || *mode_source.lock().expect("mode")),
        theme: Some(style.clone()),
    });
    let context = |is_streaming: bool, available_width: usize| MarkdownTransformContext {
        message_type: MarkdownMessageType::Assistant,
        is_streaming,
        available_width,
        message_id: None,
        timestamp: None,
    };
    let fresh = |markdown: &str, context: &MarkdownTransformContext| {
        let current = *mode.lock().expect("mode");
        let style: Arc<dyn MermaidStyle + Send + Sync> = style.clone();
        let transformer = create_mermaid_markdown_transformer(MermaidTransformerOptions {
            get_mode: Arc::new(move || current),
            theme: Some(style),
        });
        transformer(markdown, context)
    };
    let check = |markdown: &str, context: &MarkdownTransformContext| {
        assert_eq!(reused(markdown, context), fresh(markdown, context), "{markdown:?} {context:?}");
    };
    let prefixes: Vec<Vec<&str>> = documents
        .iter()
        .map(|document| {
            document.char_indices().map(|(index, _)| document[..index].trim()).chain([document.as_str()]).collect()
        })
        .collect();
    // Two messages stream at once through one transformer.
    for index in 0..prefixes[0].len().max(prefixes[1].len()) {
        for prefixes in &prefixes {
            check(prefixes[index.min(prefixes.len() - 1)], &context(true, 100));
        }
    }
    for document in &documents {
        check(document, &context(false, 100));
        check(document, &context(false, 12));
        *style.0.lock().expect("style") = "b";
        check(document, &context(false, 100));
        *mode.lock().expect("mode") = MermaidRenderingMode::Final;
        check(document, &context(true, 100));
        check(document, &context(false, 100));
        *mode.lock().expect("mode") = MermaidRenderingMode::Streaming;
        check(&document.replace("Start", "Begin"), &context(false, 100));
        check(&document[..document.len() / 2], &context(true, 100));
    }
}

#[test]
fn keeps_a_dropped_duplicate_definition_after_a_diagram() {
    let rendered = transform("[a]: u\n\n```mermaid\nflowchart LR\nA-->B\n```\n[a]: u\nnext", Options::default());
    assert!(rendered.starts_with("[a]: u\n\n`"), "{rendered:?}");
    assert!(rendered.ends_with("`\n[a]: u\nnext"), "{rendered:?}");
    assert!(!rendered.contains("```mermaid"), "{rendered:?}");
}
