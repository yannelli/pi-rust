//! LaTeX math to Unicode text, for Markdown math in the terminal (upstream `latex.ts`).
//! Unsupported input renders as `None`, so callers can show the source unchanged.

mod commands;
mod environments;
mod layout;
mod symbols;
mod tables;

use std::sync::LazyLock;

use regex::Regex;

use crate::utils::{is_js_whitespace, js_trim, js_trim_end};
use layout::{
    LAYOUT_MARKER_START, LayoutNode, NAMED_OPERATOR_END, PROTECTED_SPACE, layout_marker, normalize_output,
    render_layout_lines, trailing_layout_marker,
};

/// Marks a negative space command; the parser removes the whitespace before it.
const NEGATIVE_SPACE: &str = "\u{0}";

/// The characters of JavaScript's `\s`, for use inside a character class.
const JS_WS: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

static SCRIPT_OPERATOR_SPACING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("[{JS_WS}]*([=+-])[{JS_WS}]*")).expect("regex"));
static SIMPLE_NUMERATOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\p{L}\p{N}.]+$").expect("regex"));
static SIMPLE_DENOMINATOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\p{N}.]+$").expect("regex"));

fn lookup(table: &[(&str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(name, _)| *name == key).map(|(_, value)| *value)
}

fn contains(table: &[&str], key: &str) -> bool {
    table.contains(&key)
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn replace_characters(value: &str, table: &[(&str, &'static str)]) -> Option<String> {
    let mut result = String::new();
    for c in value.chars() {
        result.push_str(lookup(table, c.encode_utf8(&mut [0; 4]))?);
    }
    Some(result)
}

fn normalize_script_value(value: &str) -> String {
    SCRIPT_OPERATOR_SPACING.replace_all(js_trim(value), "$1").into_owned()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScriptKind {
    Sub,
    Sup,
}

fn format_unicode_script(value: &str, kind: ScriptKind) -> Option<String> {
    let table = if kind == ScriptKind::Sub { tables::SUBSCRIPTS } else { tables::SUPERSCRIPTS };
    replace_characters(&normalize_script_value(value), table)
}

fn format_script(value: &str, kind: ScriptKind) -> String {
    let value = normalize_script_value(value);
    if let Some(unicode) = format_unicode_script(&value, kind) {
        return unicode;
    }
    let prefix = if kind == ScriptKind::Sub { "_" } else { "^" };
    let all_letters = !value.is_empty() && value.chars().all(|c| c.is_ascii_alphabetic());
    if char_count(&value) == 1 || (kind == ScriptKind::Sub && all_letters) {
        return format!("{prefix}{value}");
    }
    format!("{prefix}({value})")
}

fn format_fraction(numerator: &str, denominator: &str) -> String {
    let numerator = js_trim(numerator);
    let denominator = js_trim(denominator);
    let numerator = if SIMPLE_NUMERATOR.is_match(numerator) { numerator.to_string() } else { format!("({numerator})") };
    let simple_denominator = SIMPLE_DENOMINATOR.is_match(denominator) || char_count(denominator) == 1;
    let denominator = if simple_denominator { denominator.to_string() } else { format!("({denominator})") };
    format!("{numerator}/{denominator}")
}

fn format_root(value: &str, symbol: &str) -> String {
    let value = js_trim(value);
    if SIMPLE_NUMERATOR.is_match(value) { format!("{symbol}{value}") } else { format!("{symbol}({value})") }
}

/// Recursive descent over the source characters. Nested parsers share the layout nodes.
struct LatexParser<'a> {
    source: Vec<char>,
    layout_nodes: &'a mut Vec<LayoutNode>,
    display: bool,
    position: usize,
    supported: bool,
    stack_fractions: bool,
    script_depth: usize,
}

impl<'a> LatexParser<'a> {
    fn new(source: &str, layout_nodes: &'a mut Vec<LayoutNode>, display: bool) -> Self {
        Self {
            source: source.chars().collect(),
            layout_nodes,
            display,
            position: 0,
            supported: true,
            stack_fractions: true,
            script_depth: 0,
        }
    }

    fn render(mut self) -> Option<String> {
        let rendered = self.parse_sequence(None);
        if !self.supported || self.position != self.source.len() {
            return None;
        }
        Some(normalize_output(&rendered))
    }

    fn peek(&self) -> Option<char> {
        self.source.get(self.position).copied()
    }

    fn skip_while(&mut self, predicate: impl Fn(char) -> bool) {
        while self.peek().is_some_and(&predicate) {
            self.position += 1;
        }
    }

    fn push_layout_node(&mut self, node: LayoutNode) -> String {
        self.layout_nodes.push(node);
        layout_marker(self.layout_nodes.len() - 1)
    }

    fn parse_sequence(&mut self, end_character: Option<char>) -> String {
        let mut result = String::new();
        while let Some(c) = self.peek() {
            if end_character == Some(c) {
                self.position += 1;
                return result;
            }
            match c {
                '}' => {
                    self.supported = false;
                    return result;
                }
                '{' => {
                    self.position += 1;
                    result.push_str(&self.parse_sequence(Some('}')));
                }
                '\\' => {
                    let command = self.parse_command();
                    if command == NEGATIVE_SPACE {
                        result = js_trim_end(&result).to_string();
                        if result.ends_with(NAMED_OPERATOR_END) {
                            result.pop();
                        }
                    } else {
                        result.push_str(&command);
                    }
                }
                '^' | '_' => {
                    self.position += 1;
                    result = js_trim_end(&result).to_string();
                    let script = self.parse_scripts(c);
                    if result.ends_with(NAMED_OPERATOR_END) {
                        result.pop();
                        result.push_str(&script);
                        result.push(NAMED_OPERATOR_END);
                    } else {
                        result.push_str(&script);
                    }
                }
                c if is_js_whitespace(c) => {
                    self.skip_while(is_js_whitespace);
                    result.push(' ');
                }
                '=' | '<' | '>' => {
                    result = format!("{} {c} ", js_trim_end(&result));
                    self.position += 1;
                }
                '&' => self.position += 1,
                '~' => {
                    self.position += 1;
                    result.push(' ');
                }
                _ => {
                    if c == '.'
                        && let Some(index) = trailing_layout_marker(&result)
                        && let Some(LayoutNode::Matrix { lines, .. }) = self.layout_nodes.get_mut(index)
                    {
                        if let Some(last) = lines.last_mut() {
                            last.push('.');
                        }
                        self.position += 1;
                        continue;
                    }
                    result.push(c);
                    self.position += 1;
                }
            }
        }
        if end_character.is_some() {
            self.supported = false;
        }
        result
    }

    fn parse_script_value(&mut self) -> String {
        self.script_depth += 1;
        let value = self.parse_required_argument(false);
        self.script_depth -= 1;
        value
    }

    fn parse_scripts(&mut self, initial_marker: char) -> String {
        let kind_of = |marker: char| if marker == '_' { ScriptKind::Sub } else { ScriptKind::Sup };
        let mut sub: Option<String> = None;
        let mut sup: Option<String> = None;
        let mut order = vec![kind_of(initial_marker)];
        let first = self.parse_script_value();
        match kind_of(initial_marker) {
            ScriptKind::Sub => sub = Some(first),
            ScriptKind::Sup => sup = Some(first),
        }
        let mut next = self.position;
        while self.source.get(next).copied().is_some_and(is_js_whitespace) {
            next += 1;
        }
        let second = self.source.get(next).copied().filter(|m| (*m == '^' || *m == '_') && *m != initial_marker);
        if let Some(marker) = second {
            self.position = next + 1;
            let value = self.parse_script_value();
            match kind_of(marker) {
                ScriptKind::Sub => sub = Some(value),
                ScriptKind::Sup => sup = Some(value),
            }
            order.push(kind_of(marker));
        }

        let sub_unicode = sub.as_deref().and_then(|value| format_unicode_script(value, ScriptKind::Sub));
        let sup_unicode = sup.as_deref().and_then(|value| format_unicode_script(value, ScriptKind::Sup));
        let blocks_layout = |value: &Option<String>| {
            value.as_deref().is_some_and(|value| {
                value.contains('/')
                    || (!value.contains(LAYOUT_MARKER_START)
                        && char_count(value) > 1
                        && !value.chars().any(|c| c.is_ascii_uppercase() || c == '*' || c == '∗'))
            })
        };
        let can_use_layout = !blocks_layout(&sub) && !blocks_layout(&sup);
        let needs_layout = self.display
            && can_use_layout
            && (self.script_depth > 0
                || (sub.is_some() && sub_unicode.is_none())
                || (sup.is_some() && sup_unicode.is_none()));
        if !needs_layout {
            return order
                .iter()
                .map(|kind| match kind {
                    ScriptKind::Sub => {
                        sub_unicode.clone().unwrap_or_else(|| format_script(sub.as_deref().unwrap_or(""), *kind))
                    }
                    ScriptKind::Sup => {
                        sup_unicode.clone().unwrap_or_else(|| format_script(sup.as_deref().unwrap_or(""), *kind))
                    }
                })
                .collect();
        }
        self.push_layout_node(LayoutNode::Script {
            lower: sub.as_deref().map(normalize_output),
            upper: sup.as_deref().map(normalize_output),
        })
    }

    fn parse_required_argument(&mut self, stack_fractions: bool) -> String {
        let previous = self.stack_fractions;
        self.stack_fractions = previous && stack_fractions;
        let value = self.parse_required_argument_value();
        self.stack_fractions = previous;
        value
    }

    fn parse_required_argument_value(&mut self) -> String {
        self.skip_while(is_js_whitespace);
        match self.peek() {
            None => {
                self.supported = false;
                String::new()
            }
            Some('{') => {
                self.position += 1;
                self.parse_sequence(Some('}'))
            }
            Some('\\') => self.parse_command(),
            Some(c) => {
                self.position += 1;
                c.to_string()
            }
        }
    }

    fn parse_optional_argument(&mut self) -> Option<String> {
        self.skip_while(|c| c == ' ' || c == '\t');
        if self.peek() != Some('[') {
            return None;
        }
        let Some(end) = self.source[self.position + 1..].iter().position(|c| *c == ']') else {
            self.supported = false;
            return None;
        };
        let end = self.position + 1 + end;
        let value: String = self.source[self.position + 1..end].iter().collect();
        self.position = end + 1;
        Some(self.render_nested(&value, true))
    }

    fn render_nested(&mut self, source: &str, stack_fractions: bool) -> String {
        let display = self.display && stack_fractions;
        match LatexParser::new(source, &mut *self.layout_nodes, display).render() {
            Some(rendered) => rendered,
            None => {
                self.supported = false;
                source.to_string()
            }
        }
    }
}

/// Options of [`render_latex_with`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderLatexOptions {
    /// Stack fractions and operator limits vertically for display math (default false).
    pub display: bool,
}

/// Renders a LaTeX math expression as inline Unicode text. Returns `None` when the expression
/// contains unsupported or malformed syntax.
pub fn render_latex(source: &str) -> Option<String> {
    render_latex_with(source, RenderLatexOptions::default())
}

/// [`render_latex`] with options. Display math may span several lines.
pub fn render_latex_with(source: &str, options: RenderLatexOptions) -> Option<String> {
    let mut layout_nodes = Vec::new();
    let rendered = LatexParser::new(source, &mut layout_nodes, options.display).render()?;
    if layout_nodes.is_empty() {
        return Some(rendered.replace(PROTECTED_SPACE, " "));
    }
    Some(render_layout_lines(&rendered, &layout_nodes))
}
