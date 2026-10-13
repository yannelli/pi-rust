//! Two-dimensional layout of stacked fractions, operator limits, scripts, and matrices.

use std::sync::LazyLock;

use regex::Regex;

use crate::utils::{is_js_whitespace, js_trim, js_trim_end, visible_width};

pub(super) const NAMED_OPERATOR_START: char = '\u{f0004}';
pub(super) const NAMED_OPERATOR_END: char = '\u{f0005}';
pub(super) const LAYOUT_MARKER_START: char = '\u{f0000}';
pub(super) const LAYOUT_MARKER_END: char = '\u{f0001}';
pub(super) const PROTECTED_SPACE: char = '\u{f0002}';

static LETTER_OR_NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\p{L}\p{N}]$").expect("regex"));
static LAYOUT_MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new("\u{f0000}([0-9]+)\u{f0001}").expect("regex"));
static BLANK_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new("[ \t]+").expect("regex"));

pub(super) fn is_letter_or_number(c: char) -> bool {
    LETTER_OR_NUMBER.is_match(c.encode_utf8(&mut [0; 4]))
}

/// A part of the output laid out in two dimensions, referenced from text by a layout marker.
#[derive(Debug, Clone)]
pub(super) enum LayoutNode {
    Fraction { numerator: String, denominator: String },
    Operator { operator: String, lower: Option<String>, upper: Option<String> },
    Script { lower: Option<String>, upper: Option<String> },
    Matrix { lines: Vec<String>, baseline: usize },
}

pub(super) fn layout_marker(index: usize) -> String {
    format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}")
}

/// The node index of a layout marker at the end of `text` (upstream `TRAILING_LAYOUT_MARKER_PATTERN`).
pub(super) fn trailing_layout_marker(text: &str) -> Option<usize> {
    let rest = text.strip_suffix(LAYOUT_MARKER_END)?;
    let digits_start = rest.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let digits = &rest[digits_start..];
    if digits.is_empty() || !rest[..digits_start].ends_with(LAYOUT_MARKER_START) {
        return None;
    }
    digits.parse().ok()
}

/// Puts spaces between named operators and the words next to them, then removes the operator
/// marker characters and collapses blank runs.
pub(super) fn normalize_output(value: &str) -> String {
    let mut left = String::with_capacity(value.len());
    let mut previous: Option<char> = None;
    for c in value.chars() {
        let spaced = c == NAMED_OPERATOR_START
            && previous.is_some_and(|p| is_letter_or_number(p) || matches!(p, ')' | ']' | '}' | LAYOUT_MARKER_END));
        left.push(if spaced { ' ' } else { c });
        previous = Some(c);
    }
    let left: String = left.chars().filter(|c| *c != NAMED_OPERATOR_START).collect();
    let chars: Vec<char> = left.chars().collect();
    let mut right = String::with_capacity(left.len());
    for (index, &c) in chars.iter().enumerate() {
        let spaced = c == NAMED_OPERATOR_END
            && chars.get(index + 1).is_some_and(|&n| is_letter_or_number(n) || n == '√' || n == LAYOUT_MARKER_START);
        if spaced {
            right.push(' ');
        } else if c != NAMED_OPERATOR_END {
            right.push(c);
        }
    }
    let lines: Vec<String> =
        right.split('\n').map(|line| js_trim(&BLANK_RUN.replace_all(line, " ")).to_string()).collect();
    let count = lines.len();
    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(index, line)| !line.is_empty() || (*index > 0 && *index + 1 < count))
        .map(|(_, line)| line.as_str())
        .collect();
    js_trim(&kept.join("\n")).to_string()
}

struct Layout {
    lines: Vec<String>,
    width: usize,
    baseline: usize,
}

impl Layout {
    fn text(text: String) -> Self {
        Self { width: visible_width(&text), lines: vec![text], baseline: 0 }
    }
}

fn pad_layout_line(line: &str, width: usize, centered: bool) -> String {
    let padding = width.saturating_sub(visible_width(line));
    let left = if centered { padding / 2 } else { 0 };
    format!("{}{line}{}", " ".repeat(left), " ".repeat(padding - left))
}

fn join_layouts(layouts: &[Layout]) -> Layout {
    if layouts.is_empty() {
        return Layout { lines: vec![String::new()], width: 0, baseline: 0 };
    }
    let baseline = layouts.iter().map(|layout| layout.baseline).max().unwrap_or(0) as i64;
    let below = layouts.iter().map(|layout| layout.lines.len() as i64 - layout.baseline as i64 - 1).max().unwrap_or(0);
    let mut lines = Vec::new();
    for row in 0..=(baseline + below).max(-1) {
        let mut line = String::new();
        for layout in layouts {
            let source_row = row - baseline + layout.baseline as i64;
            match usize::try_from(source_row).ok().and_then(|row| layout.lines.get(row)) {
                Some(source) => line.push_str(&pad_layout_line(source, layout.width, false)),
                None => line.push_str(&" ".repeat(layout.width)),
            }
        }
        lines.push(js_trim_end(&line).to_string());
    }
    Layout { lines, width: layouts.iter().map(|layout| layout.width).sum(), baseline: baseline as usize }
}

fn starts_with_whitespace(text: &str) -> bool {
    text.chars().next().is_some_and(is_js_whitespace)
}

fn ends_with_whitespace(text: &str) -> bool {
    text.chars().next_back().is_some_and(is_js_whitespace)
}

fn render_node(node: &LayoutNode, nodes: &[LayoutNode]) -> Layout {
    match node {
        LayoutNode::Fraction { numerator, denominator } => {
            let numerator = render_layout(numerator, nodes);
            let denominator = render_layout(denominator, nodes);
            let content_width = numerator.width.max(denominator.width).max(1);
            let width = content_width + 2;
            let mut lines: Vec<String> =
                numerator.lines.iter().map(|line| pad_layout_line(line, width, true)).collect();
            lines.push(format!(" {} ", "─".repeat(content_width)));
            lines.extend(denominator.lines.iter().map(|line| pad_layout_line(line, width, true)));
            Layout { lines, width, baseline: numerator.lines.len() }
        }
        LayoutNode::Operator { operator, lower, upper } => {
            let content_width = visible_width(operator)
                .max(lower.as_deref().map_or(0, visible_width))
                .max(upper.as_deref().map_or(0, visible_width));
            let mut lines = Vec::new();
            if let Some(upper) = upper {
                lines.push(format!("{} ", pad_layout_line(upper, content_width, true)));
            }
            lines.push(format!("{} ", pad_layout_line(operator, content_width, true)));
            if let Some(lower) = lower {
                lines.push(format!("{} ", pad_layout_line(lower, content_width, true)));
            }
            Layout { lines, width: content_width + 1, baseline: usize::from(upper.is_some()) }
        }
        LayoutNode::Script { lower, upper } => {
            let upper = upper.as_deref().map(|upper| render_layout(upper, nodes));
            let lower = lower.as_deref().map(|lower| render_layout(lower, nodes));
            let width = upper.as_ref().map_or(0, |l| l.width).max(lower.as_ref().map_or(0, |l| l.width));
            let mut lines: Vec<String> = upper
                .as_ref()
                .map(|upper| upper.lines.iter().map(|line| pad_layout_line(line, width, false)).collect())
                .unwrap_or_default();
            lines.push(" ".repeat(width));
            if let Some(lower) = &lower {
                lines.extend(lower.lines.iter().map(|line| pad_layout_line(line, width, false)));
            }
            Layout { lines, width, baseline: upper.map_or(0, |upper| upper.lines.len()) }
        }
        LayoutNode::Matrix { lines, baseline } => {
            let width = lines.iter().map(|line| visible_width(line)).max().unwrap_or(0);
            let lines = lines.iter().map(|line| pad_layout_line(line, width, false)).collect();
            Layout { lines, width, baseline: *baseline }
        }
    }
}

fn render_layout(source: &str, nodes: &[LayoutNode]) -> Layout {
    let mut rendered_lines = Vec::new();
    let mut first_baseline = 0;
    for source_line in source.split('\n') {
        let mut layouts = Vec::new();
        let mut position = 0;
        let mut previous: Option<&LayoutNode> = None;
        for captures in LAYOUT_MARKER.captures_iter(source_line) {
            let marker = captures.get(0).expect("match");
            let Some(node) = captures[1].parse::<usize>().ok().and_then(|index| nodes.get(index)) else {
                continue;
            };
            let index = marker.start();
            if index > position {
                let sliced = &source_line[position..index];
                let trimmed =
                    js_trim_end(if previous.is_some() { sliced.trim_start_matches(is_js_whitespace) } else { sliced });
                let leading = matches!(previous, Some(LayoutNode::Matrix { .. })) && starts_with_whitespace(sliced);
                let trailing = matches!(node, LayoutNode::Matrix { .. }) && ends_with_whitespace(sliced);
                let text = if !trimmed.is_empty() {
                    format!("{}{trimmed}{}", if leading { " " } else { "" }, if trailing { " " } else { "" })
                } else if leading || trailing {
                    " ".to_string()
                } else {
                    String::new()
                };
                layouts.push(Layout::text(text));
            }
            layouts.push(render_node(node, nodes));
            position = marker.end();
            previous = Some(node);
        }
        if position < source_line.len() {
            let sliced = &source_line[position..];
            let trimmed = if previous.is_some() { sliced.trim_start_matches(is_js_whitespace) } else { sliced };
            let leading = matches!(previous, Some(LayoutNode::Matrix { .. })) && starts_with_whitespace(sliced);
            layouts.push(Layout::text(if leading { format!(" {trimmed}") } else { trimmed.to_string() }));
        }
        let line_layout = join_layouts(&layouts);
        if rendered_lines.is_empty() {
            first_baseline = line_layout.baseline;
        }
        rendered_lines.extend(line_layout.lines);
    }
    let width = rendered_lines.iter().map(|line| visible_width(line)).max().unwrap_or(0);
    Layout { lines: rendered_lines, width, baseline: first_baseline }
}

/// Lays out `rendered` (text with layout markers) into lines, removing the common indentation.
pub(super) fn render_layout_lines(rendered: &str, nodes: &[LayoutNode]) -> String {
    let lines = render_layout(rendered, nodes).lines;
    let leading = |line: &str| line.chars().take_while(|c| is_js_whitespace(*c)).count();
    let indentation = lines.iter().filter(|line| !js_trim(line).is_empty()).map(|line| leading(line)).min();
    let stripped: Vec<String> = lines
        .iter()
        .map(|line| {
            let kept: String = match indentation {
                Some(indentation) => line.chars().skip(indentation).collect(),
                None => String::new(),
            };
            js_trim_end(&kept).to_string()
        })
        .collect();
    js_trim_end(&stripped.join("\n")).replace(PROTECTED_SPACE, " ")
}
