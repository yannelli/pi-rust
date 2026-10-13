//! Statement splitting, diagram kind detection and the flowchart grammar (upstream
//! `parse.ts`). Flowcharts are lenient: a bad statement keeps its parsed prefix.

use super::graph::{Edge, Graph, Group, Head, LineKind, MAX_GROUP_DEPTH, MAX_GROUPS, Shape, parse_dir};
use super::labels::{clean_label, decode_html_entities, is_id_char};
use super::strings::{first_word, non_empty, src_lines, strip_trailing, trim, words};

/// Split one source line into statements on `;`, stopping at a `%%` comment.
/// Quoted spans are opaque, so a label may contain `;` and `%%`.
fn split_statements(line: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = line.chars().collect();
    let mut cur = String::new();
    let mut in_quotes = false;
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        let trimmed = trim(cur);
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
        cur.clear();
    };
    for (i, &c) in chars.iter().enumerate() {
        if in_quotes {
            if c == '"' {
                in_quotes = false;
            }
            cur.push(c);
        } else if c == '"' {
            in_quotes = true;
            cur.push(c);
        } else if c == '%' && chars.get(i + 1) == Some(&'%') {
            break;
        } else if c == ';' {
            flush(&mut cur, out);
        } else {
            cur.push(c);
        }
    }
    flush(&mut cur, out);
}

/// All statements in a source block, in order.
pub(super) fn statements_of(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in src_lines(src) {
        split_statements(line, &mut out);
    }
    out
}

/// Diagram kind from the header statement, ASCII-lowercased.
pub(super) fn header_kind(statements: &[String]) -> Option<String> {
    let kind = first_word(statements.first()?);
    if kind.is_empty() { None } else { Some(kind.to_ascii_lowercase()) }
}

/// A diagram type this renderer draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiagramKind {
    Flowchart,
    State,
    Class,
    Er,
    Sequence,
}

impl DiagramKind {
    /// The upstream name (`"flowchart"`, `"state"`, `"class"`, `"er"`, `"sequence"`).
    pub fn as_str(self) -> &'static str {
        match self {
            DiagramKind::Flowchart => "flowchart",
            DiagramKind::State => "state",
            DiagramKind::Class => "class",
            DiagramKind::Er => "er",
            DiagramKind::Sequence => "sequence",
        }
    }
}

/// The kind of diagram `src` declares, or `None` if its header names no type this
/// renderer draws. Reads the header only, so `render(src) == None` with a known kind
/// means a syntax error.
pub fn diagram_kind(src: &str) -> Option<DiagramKind> {
    let kind = header_kind(&statements_of(src))?;
    match kind.as_str() {
        "graph" | "flowchart" => Some(DiagramKind::Flowchart),
        "erdiagram" => Some(DiagramKind::Er),
        "sequencediagram" => Some(DiagramKind::Sequence),
        k if k.starts_with("statediagram") => Some(DiagramKind::State),
        k if k.starts_with("classdiagram") => Some(DiagramKind::Class),
        _ => None,
    }
}

pub(super) fn parse_graph(src: &str) -> Option<Graph> {
    let statements = statements_of(src);
    let kind = header_kind(&statements)?;
    if kind != "graph" && kind != "flowchart" {
        return None;
    }
    let mut graph = Graph::new(parse_dir(words(&statements[0]).get(1).copied().unwrap_or("TB")));
    let mut stack: Vec<usize> = Vec::new();

    for st in &statements[1..] {
        match first_word(st).to_ascii_lowercase().as_str() {
            "subgraph" => {
                if graph.groups.len() >= MAX_GROUPS || stack.len() >= MAX_GROUP_DEPTH {
                    return None;
                }
                let (id, label) = parse_subgraph_decl(trim(&st["subgraph".len()..]));
                graph.groups.push(Group { id, label, parent: stack.last().copied() });
                stack.push(graph.groups.len() - 1);
                graph.cur_group = stack.last().copied();
                continue;
            }
            "end" => {
                stack.pop();
                graph.cur_group = stack.last().copied();
                continue;
            }
            "classdef" | "class" | "style" | "linkstyle" | "click" | "direction" => continue,
            _ => {}
        }
        parse_statement(st, &mut graph);
        if graph.over_cap {
            return None;
        }
    }
    if graph.nodes.is_empty() { None } else { Some(graph) }
}

/// `subgraph id[Title]`, `subgraph "Title"`, or a bare title.
fn parse_subgraph_decl(rest: &str) -> (String, String) {
    if let Some(quoted) = rest.strip_prefix('"')
        && let Some(close) = quoted.find('"')
    {
        let label = &quoted[..close];
        return (label.to_string(), decode_html_entities(label));
    }
    if let Some(open) = rest.find('[') {
        let id = trim(&rest[..open]);
        let label = clean_label(trim(strip_trailing(&rest[open + 1..], ']')));
        if !id.is_empty() && !label.is_empty() {
            return (id.to_string(), label);
        }
    }
    (rest.to_string(), rest.to_string())
}

fn collect(chars: &[char]) -> String {
    chars.iter().collect()
}

/// A chain of `node link node link node ...`, each link fanning out over `&`. Keeps
/// the prefix it could read and records the rest in `graph.warnings`.
fn parse_statement(st: &str, graph: &mut Graph) {
    let chars: Vec<char> = st.chars().collect();
    let Some((mut prev, next)) = parse_node_group(&chars, 0, graph) else {
        graph.warnings.push(format!("dropped, does not start with a node: \"{st}\""));
        return;
    };
    let mut i = next;
    loop {
        i = skip_spaces(&chars, i);
        if i >= chars.len() {
            break;
        }
        let Some(link) = parse_link(&chars, i) else {
            graph.warnings.push(format!("dropped, expected a link: \"{}\"", collect(&chars[i..])));
            break;
        };
        i = skip_spaces(&chars, link.next);
        let Some((target, next)) = parse_node_group(&chars, i, graph) else {
            graph.warnings.push(format!("dropped, link has no target: \"{st}\""));
            break;
        };
        i = next;
        // `A <-- B` reads right to left: swap the endpoints so the head becomes forward.
        let reversed = link.left == Head::Arrow && link.right != Head::Arrow;
        for &f in &prev {
            for &t in &target {
                let pushed = graph.push_edge(Edge {
                    from: if reversed { t } else { f },
                    to: if reversed { f } else { t },
                    label: link.label.clone(),
                    head_to: if reversed { Head::Arrow } else { link.right },
                    head_from: if reversed { link.right } else { link.left },
                    line: link.line,
                });
                if !pushed {
                    return;
                }
            }
        }
        prev = target;
    }
}

/// One or more nodes joined by `&`, which fan out into a cross product.
fn parse_node_group(chars: &[char], start: usize, graph: &mut Graph) -> Option<(Vec<usize>, usize)> {
    let (first, mut i) = parse_node(chars, start, graph)?;
    let mut group = vec![first];
    loop {
        let j = skip_spaces(chars, i);
        if chars.get(j) != Some(&'&') {
            break;
        }
        let (next, after) = parse_node(chars, j + 1, graph)?;
        group.push(next);
        i = after;
    }
    Some((group, i))
}

fn skip_spaces(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
        i += 1;
    }
    i
}

/// End of a `:::name` style tag starting at `at`; a name never ends in `-`, so
/// `A:::x-->B` keeps its link.
pub(super) fn style_tag_end(chars: &[char], at: usize) -> Option<usize> {
    if chars.get(at..at + 3) != Some(&[':', ':', ':']) {
        return None;
    }
    let mut k = at + 3;
    while k < chars.len() && (is_id_char(chars[k]) || chars[k] == '-') {
        k += 1;
    }
    while k > at + 3 && chars[k - 1] == '-' {
        k -= 1;
    }
    (k > at + 3).then_some(k)
}

fn parse_node(chars: &[char], start: usize, graph: &mut Graph) -> Option<(usize, usize)> {
    let mut i = skip_spaces(chars, start);
    let id_start = i;
    while i < chars.len() && is_id_char(chars[i]) {
        i += 1;
    }
    if i == id_start {
        return None;
    }
    let id = collect(&chars[id_start..i]);
    let shaped = read_shape_at(chars, i);
    if let Some(unclosed) = shaped.unclosed {
        graph.warnings.push(format!("node \"{id}\": label is missing its closing `{unclosed}`"));
    }
    let index = graph.node_index(&id, shaped.label, shaped.shape)?;
    // `id:::name` attaches a style class; swallow it so the statement keeps parsing.
    let next = style_tag_end(chars, shaped.after).unwrap_or(shaped.after);
    Some((index, next))
}

/// What a shape bracket yielded; `unclosed` is the closer that was never found.
struct Shaped {
    shape: Shape,
    label: Option<String>,
    after: usize,
    unclosed: Option<&'static str>,
}

/// Dispatch on the bracket following an id to pick shape and closing token.
fn read_shape_at(chars: &[char], i: usize) -> Shaped {
    match (chars.get(i), chars.get(i + 1)) {
        (Some('['), Some('[')) => read_shape(chars, i + 2, "]]", Shape::Rect),
        (Some('['), Some('(')) => read_shape(chars, i + 2, ")]", Shape::Round),
        (Some('['), _) => read_shape(chars, i + 1, "]", Shape::Rect),
        (Some('('), Some('(')) => read_shape(chars, i + 2, "))", Shape::Round),
        (Some('('), Some('[')) => read_shape(chars, i + 2, "])", Shape::Round),
        (Some('('), _) => read_shape(chars, i + 1, ")", Shape::Round),
        (Some('{'), Some('{')) => read_shape(chars, i + 2, "}}", Shape::Diamond),
        (Some('{'), _) => read_shape(chars, i + 1, "}", Shape::Diamond),
        (Some('>'), _) => read_shape(chars, i + 1, "]", Shape::Rect),
        _ => Shaped { shape: Shape::Rect, label: None, after: i, unclosed: None },
    }
}

/// Read label text up to `closer`. If the first non-space character is a quote, the
/// closer is ignored inside quotes, so `A["a] b"]` is one node.
fn read_shape(chars: &[char], start: usize, closer: &'static str, shape: Shape) -> Shaped {
    let closer_chars: Vec<char> = closer.chars().collect();
    let mut j = start;
    while matches!(chars.get(j), Some(' ' | '\t')) {
        j += 1;
    }
    let quoted = chars.get(j) == Some(&'"');
    let mut text = String::new();
    let mut in_quotes = false;
    for i in start..chars.len() {
        let c = chars[i];
        if quoted && c == '"' {
            in_quotes = !in_quotes;
            text.push(c);
            continue;
        }
        if !in_quotes && chars[i..].starts_with(&closer_chars) {
            return Shaped { shape, label: Some(clean_label(&text)), after: i + closer_chars.len(), unclosed: None };
        }
        text.push(c);
    }
    // Ran off the end: everything after the bracket became label text.
    Shaped { shape, label: Some(clean_label(&text)), after: chars.len(), unclosed: Some(closer) }
}

fn is_link_char(c: char) -> bool {
    matches!(c, '-' | '.' | '=' | '<' | '>')
}

struct Link {
    left: Head,
    right: Head,
    line: LineKind,
    label: Option<String>,
    next: usize,
}

/// Read a link operator and its label: `-->|text|`, or the inline `-- text -->` when
/// the first operator carried no head.
fn parse_link(chars: &[char], start: usize) -> Option<Link> {
    let at = |k: usize| chars.get(k).copied();
    let mut i = skip_spaces(chars, start);
    let mut left = Head::None;
    // A leading `o`/`x` decorates the tail, but only directly before an operator.
    if matches!(at(i), Some('o' | 'x')) && matches!(at(i + 1), Some('-' | '.' | '=')) {
        left = if at(i) == Some('o') { Head::Circle } else { Head::Cross };
        i += 1;
    }

    let op_start = i;
    while i < chars.len() && is_link_char(chars[i]) {
        i += 1;
    }
    if i == op_start {
        return None;
    }
    let op1 = collect(&chars[op_start..i]);
    if left == Head::None && op1.starts_with('<') {
        left = Head::Arrow;
    }
    let mut line = line_kind(&op1);
    let mut right = if op1.contains('>') { Head::Arrow } else { Head::None };
    if right == Head::None
        && let Some((head, next)) = trailing_head(chars, i)
    {
        right = head;
        i = next;
    }

    if at(i) == Some('|') {
        i += 1;
        let label_start = i;
        while i < chars.len() && chars[i] != '|' {
            i += 1;
        }
        let label = clean_label(&collect(&chars[label_start..i]));
        if at(i) == Some('|') {
            i += 1;
        }
        return Some(Link { left, right, line, label: non_empty(label), next: i });
    }

    if right == Head::None {
        let text_start = skip_spaces(chars, i);
        let mut j = text_start;
        while j < chars.len() && !is_link_char(chars[j]) {
            j += 1;
        }
        if j < chars.len() && j > text_start && chars[j] != '<' {
            let text = collect(&chars[text_start..j]);
            let op2_start = j;
            while j < chars.len() && is_link_char(chars[j]) {
                j += 1;
            }
            let op2 = collect(&chars[op2_start..j]);
            if op2.contains('>') {
                right = Head::Arrow;
            } else if let Some((head, next)) = trailing_head(chars, j) {
                right = head;
                j = next;
            }
            if line == LineKind::Solid {
                line = line_kind(&op2);
            }
            return Some(Link { left, right, line, label: non_empty(clean_label(&text)), next: j });
        }
    }
    Some(Link { left, right, line, label: None, next: i })
}

fn line_kind(op: &str) -> LineKind {
    if op.contains('=') {
        LineKind::Thick
    } else if op.contains('.') {
        LineKind::Dotted
    } else {
        LineKind::Solid
    }
}

/// A trailing `o`/`x` head, only when followed by a statement boundary.
fn trailing_head(chars: &[char], i: usize) -> Option<(Head, usize)> {
    let head = match chars.get(i) {
        Some('o') => Head::Circle,
        Some('x') => Head::Cross,
        _ => return None,
    };
    matches!(chars.get(i + 1), None | Some(' ' | '\t' | '|' | '&' | ';')).then_some((head, i + 1))
}
