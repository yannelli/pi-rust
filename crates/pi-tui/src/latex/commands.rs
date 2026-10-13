//! Backslash commands and operators with limits.

use super::layout::{LayoutNode, NAMED_OPERATOR_END, NAMED_OPERATOR_START, normalize_output};
use super::symbols::SYMBOLS;
use super::tables::{
    ACCENTS, BLACKBOARD, DISPLAY_LIMIT_SYMBOLS, FONT_SWITCH_COMMANDS, IGNORED_COMMANDS, LIMIT_OPERATORS,
    NAMED_OPERATORS, NEGATED_SYMBOLS, NEGATIVE_SPACING_COMMANDS, PLAIN_WRAPPERS, RELATION_COMMANDS, SIZE_COMMANDS,
    SPACING_COMMANDS,
};
use super::{
    LatexParser, NEGATIVE_SPACE, ScriptKind, char_count, contains, format_fraction, format_root, format_script, lookup,
};
use crate::utils::{is_js_whitespace, js_trim};

/// How an inline operator shows its lower limit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LowerStyle {
    Bracket,
    Script,
}

impl LatexParser<'_> {
    pub(super) fn parse_command(&mut self) -> String {
        self.position += 1;
        let Some(first) = self.peek() else {
            self.supported = false;
            return String::new();
        };
        if first == '\n' || first == '\r' {
            self.position += 1;
            if first == '\r' && self.peek() == Some('\n') {
                self.position += 1;
            }
            return " ".into();
        }
        let command: String = if first.is_ascii_alphabetic() {
            let start = self.position;
            self.skip_while(|c| c.is_ascii_alphabetic());
            self.source[start..self.position].iter().collect()
        } else {
            self.position += 1;
            first.to_string()
        };
        let command = command.as_str();

        if command == "\\" {
            return "\n".into();
        }
        if contains(SPACING_COMMANDS, command) {
            return " ".into();
        }
        if contains(NEGATIVE_SPACING_COMMANDS, command) {
            return NEGATIVE_SPACE.into();
        }
        if contains(FONT_SWITCH_COMMANDS, command) {
            self.skip_while(is_js_whitespace);
            return String::new();
        }
        if contains(IGNORED_COMMANDS, command) {
            return String::new();
        }
        if matches!(command, "{" | "}" | "$" | "%" | "#" | "_" | "&") {
            return command.into();
        }
        if command == "|" {
            return "‖".into();
        }
        if command == "not" {
            return self.parse_not();
        }
        if contains(LIMIT_OPERATORS, command) {
            return self.parse_operator(command, LowerStyle::Bracket, true, true);
        }
        if let Some(symbol) = lookup(SYMBOLS, command) {
            if contains(DISPLAY_LIMIT_SYMBOLS, command) {
                return self.parse_operator(symbol, LowerStyle::Script, true, false);
            }
            let spaced = command == "cdot" || command == "times" || contains(RELATION_COMMANDS, command);
            return if spaced { format!(" {symbol} ") } else { symbol.into() };
        }
        if contains(NAMED_OPERATORS, command) {
            return format!("{NAMED_OPERATOR_START}{command}{NAMED_OPERATOR_END}");
        }
        if contains(SIZE_COMMANDS, command) {
            return String::new();
        }
        if matches!(command, "left" | "middle" | "right") {
            if self.peek() == Some('.') {
                self.position += 1;
            }
            return String::new();
        }
        match command {
            "frac" | "dfrac" | "tfrac" => self.parse_fraction(command),
            "sqrt" => self.parse_sqrt(),
            "boxed" | "fbox" => format!("[{}]", js_trim(&self.parse_required_argument(true))),
            "binom" | "dbinom" | "tbinom" => {
                let n = self.parse_required_argument(true);
                let k = self.parse_required_argument(true);
                format!("({n} choose {k})")
            }
            "mathbb" => {
                let value = self.parse_required_argument(true);
                value
                    .chars()
                    .map(|c| lookup(BLACKBOARD, c.encode_utf8(&mut [0; 4])).map_or(c.to_string(), str::to_string))
                    .collect()
            }
            "operatorname" => {
                let starred = self.peek() == Some('*');
                if starred {
                    self.position += 1;
                }
                let operator = normalize_output(&self.parse_required_argument(true));
                self.parse_operator(js_trim(&operator), LowerStyle::Bracket, starred, true)
            }
            "mod" | "bmod" => " mod ".into(),
            "pmod" | "pod" => {
                let value = self.parse_required_argument(true);
                let value = js_trim(&value);
                if command == "pmod" { format!(" (mod {value})") } else { format!(" ({value})") }
            }
            "overset" | "stackrel" | "underset" => {
                let script = self.parse_required_argument(true);
                let value = self.parse_required_argument(true);
                let kind = if command == "underset" { ScriptKind::Sub } else { ScriptKind::Sup };
                format!("{}{}", js_trim(&value), format_script(&script, kind))
            }
            "begin" => self.parse_environment(),
            "end" => {
                self.supported = false;
                String::new()
            }
            _ => self.parse_other_command(command),
        }
    }

    fn parse_other_command(&mut self, command: &str) -> String {
        if let Some(accent) = lookup(ACCENTS, command) {
            let value = self.parse_required_argument(true);
            return if char_count(&value) == 1 { format!("{value}{accent}") } else { format!("{command}({value})") };
        }
        if contains(PLAIN_WRAPPERS, command) {
            let value = self.parse_required_argument(true);
            return if command.starts_with("text") || command == "mbox" { value } else { js_trim(&value).into() };
        }
        self.supported = false;
        format!("\\{command}")
    }

    fn parse_not(&mut self) -> String {
        let value = self.parse_required_argument(false);
        let value = js_trim(&value);
        if let Some(negated) = lookup(NEGATED_SYMBOLS, value) {
            return format!(" {negated} ");
        }
        let mut chars = value.chars();
        let Some(first) = chars.next() else {
            self.supported = false;
            return String::new();
        };
        format!(" {first}\u{338}{} ", chars.as_str())
    }

    fn parse_fraction(&mut self, command: &str) -> String {
        let should_stack = self.display && self.stack_fractions && command != "tfrac";
        let numerator = self.parse_required_argument(!should_stack);
        let denominator = self.parse_required_argument(!should_stack);
        if should_stack {
            return self.push_layout_node(LayoutNode::Fraction {
                numerator: normalize_output(&numerator),
                denominator: normalize_output(&denominator),
            });
        }
        format_fraction(&numerator, &denominator)
    }

    fn parse_sqrt(&mut self) -> String {
        let degree = self.parse_optional_argument();
        let value = self.parse_required_argument(true);
        match degree.as_deref().map(js_trim) {
            None | Some("2") => format_root(&value, "√"),
            Some("3") => format_root(&value, "∛"),
            Some("4") => format_root(&value, "∜"),
            Some(degree) => format!("{}{}", format_script(degree, ScriptKind::Sup), format_root(&value, "√")),
        }
    }

    fn skip_blanks_from(&self, mut position: usize) -> usize {
        while self.source.get(position).is_some_and(|c| *c == ' ' || *c == '\t') {
            position += 1;
        }
        position
    }

    /// `\limits` or `\nolimits` (not followed by a letter) at `position`, with its length.
    fn limits_modifier(&self, position: usize) -> Option<(bool, usize)> {
        for (name, limits) in [("limits", true), ("nolimits", false)] {
            let end = position + 1 + name.len();
            let matches = self.source.get(position) == Some(&'\\')
                && self.source.get(position + 1..end).is_some_and(|word| word.iter().copied().eq(name.chars()))
                && !self.source.get(end).is_some_and(|c| c.is_ascii_alphabetic());
            if matches {
                return Some((limits, name.len() + 1));
            }
        }
        None
    }

    fn parse_operator(
        &mut self,
        operator: &str,
        lower_style: LowerStyle,
        display_limits: bool,
        spaced: bool,
    ) -> String {
        let mut use_display_limits = display_limits;
        let modifier_position = self.skip_blanks_from(self.position);
        if let Some((limits, length)) = self.limits_modifier(modifier_position) {
            use_display_limits = limits;
            self.position = modifier_position + length;
        }

        let mut lower: Option<String> = None;
        let mut upper: Option<String> = None;
        loop {
            let script_position = self.skip_blanks_from(self.position);
            let kind = self.source.get(script_position).copied();
            if kind != Some('_') && kind != Some('^') {
                break;
            }
            self.position = script_position + 1;
            let value = normalize_output(&self.parse_required_argument(false)).replace(' ', "");
            let slot = if kind == Some('_') { &mut lower } else { &mut upper };
            if slot.is_some() {
                self.supported = false;
            }
            *slot = Some(value);
        }

        if self.display && use_display_limits && (lower.is_some() || upper.is_some()) {
            return self.push_layout_node(LayoutNode::Operator { operator: operator.into(), lower, upper });
        }
        let mut rendered = operator.to_string();
        if let Some(lower) = &lower {
            match lower_style {
                LowerStyle::Bracket => rendered.push_str(&format!("[{lower}]")),
                LowerStyle::Script => rendered.push_str(&format_script(lower, ScriptKind::Sub)),
            }
        }
        if let Some(upper) = &upper {
            rendered.push_str(&format_script(upper, ScriptKind::Sup));
        }
        if spaced { format!(" {rendered} ") } else { rendered }
    }
}
