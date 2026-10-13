//! `\begin{...}` environments: equations, aligned rows, cases, and matrices.

use std::sync::LazyLock;

use regex::Regex;

use super::layout::{LayoutNode, PROTECTED_SPACE};
use super::{JS_WS, LatexParser};
use crate::utils::{js_trim, visible_width};

static ROW_SEPARATOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\\\(?:\[[^\]\n]*\])?").expect("regex"));
static LEADING_GROUP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^[{JS_WS}]*\\{{[^}}]*\\}}")).expect("regex"));
static TRAILING_COMMA: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(",[{JS_WS}]*$")).expect("regex"));

const ALIGNED_ENVIRONMENTS: [&str; 11] = [
    "aligned",
    "align",
    "align*",
    "alignedat",
    "alignat",
    "alignat*",
    "gather",
    "gathered",
    "multline",
    "multline*",
    "split",
];
const MATRIX_ENVIRONMENTS: [&str; 8] =
    ["array", "matrix", "smallmatrix", "pmatrix", "bmatrix", "Bmatrix", "vmatrix", "Vmatrix"];

fn split_rows(body: &str) -> Vec<&str> {
    ROW_SEPARATOR.split(body).collect()
}

fn protected_padding(count: usize) -> String {
    std::iter::repeat_n(PROTECTED_SPACE, count).collect()
}

/// Whether a `cases` condition starts with a word that needs no "if" (JavaScript `/^(?:if|...)\b/i`).
fn has_condition_word(condition: &str) -> bool {
    ["if", "when", "for", "otherwise"].iter().any(|word| {
        condition.get(..word.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(word))
            && !condition[word.len()..].chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

impl LatexParser<'_> {
    fn read_raw_group(&mut self) -> Option<String> {
        self.skip_while(|c| c == ' ' || c == '\t');
        if self.peek() != Some('{') {
            self.supported = false;
            return None;
        }
        self.position += 1;
        let start = self.position;
        let mut depth = 1;
        while let Some(c) = self.peek() {
            if c == '\\' {
                self.position += 2;
                continue;
            }
            if c == '{' {
                depth += 1;
            }
            if c == '}' {
                depth -= 1;
            }
            if depth == 0 {
                let value = self.source[start..self.position].iter().collect();
                self.position += 1;
                return Some(value);
            }
            self.position += 1;
        }
        self.supported = false;
        None
    }

    fn find_from(&self, needle: &str, from: usize) -> Option<usize> {
        let needle: Vec<char> = needle.chars().collect();
        (from..self.source.len()).find(|&start| self.source[start..].starts_with(&needle))
    }

    pub(super) fn parse_environment(&mut self) -> String {
        let environment = match self.read_raw_group() {
            Some(environment) if !environment.is_empty() => environment,
            _ => return String::new(),
        };
        let end_marker = format!("\\end{{{environment}}}");
        let Some(end) = self.find_from(&end_marker, self.position) else {
            self.supported = false;
            return String::new();
        };
        let body: String = self.source[self.position..end].iter().collect();
        self.position = end + end_marker.chars().count();
        let environment = environment.as_str();

        if matches!(environment, "equation" | "equation*" | "displaymath") {
            return js_trim(&self.render_nested(&body, true)).to_string();
        }
        if ALIGNED_ENVIRONMENTS.contains(&environment) {
            let aligned_at = matches!(environment, "alignedat" | "alignat" | "alignat*");
            let body = if aligned_at { LEADING_GROUP.replace(&body, "").into_owned() } else { body };
            let rows: Vec<String> = split_rows(&body)
                .into_iter()
                .map(|row| {
                    let cells: Vec<&str> = row.split('&').collect();
                    let source = if aligned_at {
                        cells.chunks(2).map(|pair| pair.concat()).collect::<Vec<_>>().join(" ")
                    } else {
                        cells.concat()
                    };
                    js_trim(&self.render_nested(&source, true)).to_string()
                })
                .filter(|row| !row.is_empty())
                .collect();
            return rows.join("\n");
        }
        if environment == "cases" || environment == "cases*" {
            return self.render_cases(&body);
        }
        if MATRIX_ENVIRONMENTS.contains(&environment) {
            let body = if environment == "array" { LEADING_GROUP.replace(&body, "").into_owned() } else { body };
            return self.render_matrix(environment, &body);
        }
        self.supported = false;
        body
    }

    fn render_cells(&mut self, body: &str) -> Vec<Vec<String>> {
        split_rows(body)
            .into_iter()
            .map(|row| row.split('&').map(|cell| js_trim(&self.render_nested(cell, false)).to_string()).collect())
            .filter(|row: &Vec<String>| row.iter().any(|cell| !cell.is_empty()))
            .collect()
    }

    fn render_cases(&mut self, body: &str) -> String {
        let rows = self.render_cells(body);
        let value_of =
            |row: &Vec<String>| TRAILING_COMMA.replace(row.first().map_or("", String::as_str), "").into_owned();
        let value_width = rows.iter().map(|row| visible_width(&value_of(row))).max().unwrap_or(0);
        let contents: Vec<String> = rows
            .iter()
            .map(|row| {
                let value = value_of(row);
                let condition = row.get(1).map_or("", String::as_str);
                if condition.is_empty() {
                    return value;
                }
                let prefix = if has_condition_word(condition) { " " } else { " if " };
                let padding = protected_padding(value_width.saturating_sub(visible_width(&value)));
                format!("{value}{padding}{prefix}{condition}")
            })
            .collect();
        if contents.len() <= 1 {
            return contents.first().map_or(String::new(), |content| format!("⎧ {content}"));
        }

        let middle = contents.len() / 2;
        let mut visual_rows: Vec<Option<String>> = contents.into_iter().map(Some).collect();
        if visual_rows.len() % 2 == 0 {
            visual_rows.insert(middle, None);
        }
        let last = visual_rows.len() - 1;
        let lines = visual_rows
            .into_iter()
            .enumerate()
            .map(|(index, content)| {
                let delimiter = if index == 0 {
                    "⎧"
                } else if index == last {
                    "⎩"
                } else {
                    "⎨"
                };
                content.map_or(delimiter.to_string(), |content| format!("{delimiter} {content}"))
            })
            .collect();
        self.push_layout_node(LayoutNode::Matrix { lines, baseline: middle })
    }

    fn render_matrix(&mut self, environment: &str, body: &str) -> String {
        let matrix = self.render_cells(body);
        let column_count = matrix.iter().map(Vec::len).max().unwrap_or(0);
        let column_widths: Vec<usize> = (0..column_count)
            .map(|column| {
                matrix.iter().map(|row| row.get(column).map_or(0, |cell| visible_width(cell))).max().unwrap_or(0)
            })
            .collect();
        let rows: Vec<String> = matrix
            .iter()
            .map(|row| {
                (0..column_count)
                    .map(|column| {
                        let cell = row.get(column).map_or("", String::as_str);
                        format!(
                            "{cell}{}",
                            protected_padding(column_widths[column].saturating_sub(visible_width(cell)))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" │ ")
            })
            .collect();

        let lines: Vec<String> = if matches!(environment, "array" | "matrix" | "smallmatrix") {
            rows
        } else {
            let delimiter: [&str; 6] = match environment {
                "pmatrix" => ["⎛", "⎞", "⎜", "⎟", "⎝", "⎠"],
                "bmatrix" => ["⎡", "⎤", "⎢", "⎥", "⎣", "⎦"],
                "Bmatrix" => ["⎧", "⎫", "⎨", "⎬", "⎩", "⎭"],
                "vmatrix" => ["│"; 6],
                "Vmatrix" => ["║"; 6],
                _ => {
                    self.supported = false;
                    return rows.join("\n");
                }
            };
            let last = rows.len().saturating_sub(1);
            rows.iter()
                .enumerate()
                .map(|(index, row)| {
                    let (left, right) = if index == 0 {
                        (delimiter[0], delimiter[1])
                    } else if index == last {
                        (delimiter[4], delimiter[5])
                    } else {
                        (delimiter[2], delimiter[3])
                    };
                    format!("{left} {row} {right}")
                })
                .collect()
        };
        if lines.len() <= 1 {
            return lines.into_iter().next().unwrap_or_default();
        }
        self.push_layout_node(LayoutNode::Matrix { lines, baseline: 0 })
    }
}
