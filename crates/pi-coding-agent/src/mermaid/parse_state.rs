//! State and class diagram grammars (upstream `parse.ts`). Unlike flowcharts, one
//! unreadable statement fails the whole diagram.

use super::graph::{ClassInfo, Dir, Edge, Graph, Head, LineKind, MAX_EDGES, MAX_MEMBERS, Shape, parse_dir};
use super::labels::{ELLIPSIS, decode_html_entities, is_id_char};
use super::parse_flow::{header_kind, statements_of, style_tag_end};
use super::strings::{first_word, has_space, non_empty, strip_trailing, trim, trim_end, trim_start, words};

pub(super) fn parse_state(src: &str) -> Option<Graph> {
    let statements = statements_of(src);
    if !header_kind(&statements)?.starts_with("statediagram") {
        return None;
    }
    let mut graph = Graph::new(Dir::Down);
    let mut in_note = false;

    for raw in &statements[1..] {
        if in_note {
            if raw.eq_ignore_ascii_case("end note") {
                in_note = false;
            }
            continue;
        }
        let st = drop_style_tags(raw);
        match first_word(&st).to_ascii_lowercase().as_str() {
            "direction" => graph.dir = parse_dir(words(&st).get(1).copied().unwrap_or("")),
            // A single-line `note ... : text` needs no terminator.
            "note" => in_note = !st.contains(':'),
            "state" => parse_state_decl(&st, &mut graph)?,
            // Styling and composite-state punctuation carry no layout meaning.
            "classdef" | "class" | "hide" | "scale" | "}" | "--" => {}
            _ if st.contains("-->") => parse_transition(&st, &mut graph)?,
            _ => parse_state_desc(&st, &mut graph)?,
        }
        if graph.over_cap {
            return None;
        }
    }
    if graph.nodes.is_empty() { None } else { Some(graph) }
}

/// `state "Label" as id`, `state id <<choice>>`, or `state id {`.
fn parse_state_decl(st: &str, graph: &mut Graph) -> Option<()> {
    let rest = trim(&st["state".len()..]);
    let rest = trim(rest.strip_suffix('{').unwrap_or(rest));
    if rest.is_empty() {
        return Some(());
    }
    if let Some(quoted) = rest.strip_prefix('"') {
        let close = quoted.find('"')?;
        let label = &quoted[..close];
        let after = trim(&quoted[close + 1..]);
        let id = match after.strip_prefix("as") {
            Some(id) => trim(id),
            None => label,
        };
        return graph.node_label(id, decode_html_entities(label)).map(drop);
    }

    let mut shape = Shape::Round;
    let mut id = rest;
    let mut stereotyped = false;
    if let Some(pos) = rest.find("<<") {
        let stereo = &rest[pos + 2..];
        if trim(stereo.strip_suffix(">>").unwrap_or(stereo)) == "choice" {
            shape = Shape::Diamond;
        }
        id = trim(&rest[..pos]);
        stereotyped = true;
    }
    if id.is_empty() || has_space(id) {
        return None;
    }
    graph.node_index(id, stereotyped.then(|| id.to_string()), shape).map(drop)
}

/// `A --> B: label`, including chains `A --> B --> C`.
fn parse_transition(st: &str, graph: &mut Graph) -> Option<()> {
    let mut rest = st;
    let mut prev: Option<usize> = None;
    while let Some((lhs, rhs)) = rest.split_once("-->") {
        let from_id = trim(strip_trailing(trim_end(lhs), '-'));
        let from = match prev {
            // Mid-chain the source is the last target, so nothing may come before it.
            Some(prev) if from_id.is_empty() => prev,
            Some(_) => return None,
            None if from_id.is_empty() => return None,
            None => state_endpoint(graph, from_id, true)?,
        };

        let (to_part_raw, tail) = match rhs.find("-->") {
            Some(n) => (&rhs[..n], &rhs[n..]),
            None => (rhs, ""),
        };
        let (to_part, label) = match to_part_raw.split_once(':') {
            Some((to_part, label)) => (to_part, non_empty(decode_html_entities(trim(label)))),
            None => (to_part_raw, None),
        };
        let to_id = trim(strip_trailing(trim_end(trim_start(to_part).trim_start_matches('>')), '-'));
        if to_id.is_empty() {
            return None;
        }
        let to = state_endpoint(graph, to_id, false)?;
        let edge = Edge { from, to, label, head_to: Head::Arrow, head_from: Head::None, line: LineKind::Solid };
        if !graph.push_edge(edge) {
            return Some(());
        }
        prev = Some(to);
        rest = tail;
    }
    Some(())
}

/// Remove every `:::name` style tag from a statement before dispatch, since the `:`
/// label split would otherwise cut inside a `:::`.
pub(super) fn drop_style_tags(st: &str) -> String {
    let chars: Vec<char> = st.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if let Some(end) = style_tag_end(&chars, i) {
            i = end;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `[*]` is start or end depending on which side of the arrow it sits.
fn state_endpoint(graph: &mut Graph, id: &str, is_source: bool) -> Option<usize> {
    if id == "[*]" {
        let key = if is_source { "[*]start" } else { "[*]end" };
        return graph.node_index(key, Some("\u{25cf}".to_string()), Shape::Round);
    }
    graph.node_index(id, None, Shape::Round)
}

/// `id: description`, or a bare state name.
fn parse_state_desc(st: &str, graph: &mut Graph) -> Option<()> {
    if let Some((id, desc)) = st.split_once(':') {
        let (id, desc) = (trim(id), trim(desc));
        if id.is_empty() || has_space(id) || desc.is_empty() {
            return None;
        }
        return graph.node_label(id, decode_html_entities(desc)).map(drop);
    }
    if has_space(st) {
        return None;
    }
    graph.node_index(st, None, Shape::Round).map(drop)
}

/// Relation operators, longest first so `--|>` wins over `--`.
const CLASS_OPS: [(&str, Head, Head, LineKind); 14] = [
    ("<|--", Head::Triangle, Head::None, LineKind::Solid),
    ("--|>", Head::None, Head::Triangle, LineKind::Solid),
    ("<|..", Head::Triangle, Head::None, LineKind::Dotted),
    ("..|>", Head::None, Head::Triangle, LineKind::Dotted),
    ("*--", Head::DiamondFill, Head::None, LineKind::Solid),
    ("--*", Head::None, Head::DiamondFill, LineKind::Solid),
    ("o--", Head::DiamondOpen, Head::None, LineKind::Solid),
    ("--o", Head::None, Head::DiamondOpen, LineKind::Solid),
    ("<--", Head::Arrow, Head::None, LineKind::Solid),
    ("-->", Head::None, Head::Arrow, LineKind::Solid),
    ("<..", Head::Arrow, Head::None, LineKind::Dotted),
    ("..>", Head::None, Head::Arrow, LineKind::Dotted),
    ("--", Head::None, Head::None, LineKind::Solid),
    ("..", Head::None, Head::None, LineKind::Dotted),
];

const MAX_CLASS_OP: usize = 4;

/// Declare a class, keeping `infos` aligned with `graph.nodes`.
fn declare(graph: &mut Graph, infos: &mut Vec<ClassInfo>, name: &str) -> Option<usize> {
    let idx = graph.node_index(name, None, Shape::Rect);
    infos.resize_with(infos.len().max(graph.nodes.len()), ClassInfo::default);
    idx
}

pub(super) fn parse_class(src: &str) -> Option<(Graph, Vec<ClassInfo>)> {
    let statements = statements_of(src);
    if !header_kind(&statements)?.starts_with("classdiagram") {
        return None;
    }
    let mut graph = Graph::new(Dir::Down);
    let mut infos: Vec<ClassInfo> = Vec::new();
    let mut cur_class: Option<usize> = None;

    for raw in &statements[1..] {
        if let Some(class) = cur_class {
            if raw == "}" {
                cur_class = None;
            } else {
                push_member(&mut infos[class], raw);
            }
            continue;
        }
        let st = drop_style_tags(raw);
        let first = first_word(&st).to_ascii_lowercase();
        match first.as_str() {
            "direction" => {
                graph.dir = parse_dir(words(&st).get(1).copied().unwrap_or(""));
                continue;
            }
            "note" | "callback" | "click" | "link" | "style" | "cssclass" | "classdef" | "namespace" | "}" => continue,
            "class" => {
                let rest = trim(&st["class".len()..]);
                let open = rest.strip_suffix('{');
                let name = open.map_or(rest, trim);
                if name.is_empty() || has_space(name) {
                    return None;
                }
                let idx = declare(&mut graph, &mut infos, name)?;
                if open.is_some() {
                    cur_class = Some(idx);
                }
                continue;
            }
            _ => {}
        }

        if let Some(after) = st.strip_prefix("<<") {
            let (annotation, name) = after.split_once(">>")?;
            let name = trim(name);
            if name.is_empty() || has_space(name) {
                return None;
            }
            let idx = declare(&mut graph, &mut infos, name)?;
            infos[idx].annotation = Some(trim(annotation).to_string());
            continue;
        }

        if let Some(rel) = parse_class_relation(&st) {
            let from = declare(&mut graph, &mut infos, &rel.from)?;
            let to = declare(&mut graph, &mut infos, &rel.to)?;
            if graph.edges.len() >= MAX_EDGES {
                return None;
            }
            let ClassRelation { head_from, head_to, line, label, .. } = rel;
            graph.edges.push(Edge { from, to, label, head_to, head_from, line });
            continue;
        }

        let (id, text) = st.split_once(':')?;
        let (id, text) = (trim(id), trim(text));
        if id.is_empty() || has_space(id) || text.is_empty() {
            return None;
        }
        let idx = declare(&mut graph, &mut infos, id)?;
        push_member(&mut infos[idx], text);
    }

    if graph.nodes.is_empty() {
        return None;
    }
    infos.resize_with(infos.len().max(graph.nodes.len()), ClassInfo::default);
    Some((graph, infos))
}

/// Append to a compartment; everything past the cap becomes one ellipsis row.
pub(super) fn push_capped(list: &mut Vec<String>, item: String) {
    if list.len() < MAX_MEMBERS {
        list.push(item);
    } else if list.len() == MAX_MEMBERS {
        list.push(ELLIPSIS.to_string());
    }
}

/// Add a member to the attribute or method compartment.
fn push_member(info: &mut ClassInfo, raw: &str) {
    if let Some(after) = raw.strip_prefix("<<") {
        if let Some((annotation, _)) = after.split_once(">>") {
            info.annotation = Some(trim(annotation).to_string());
        }
        return;
    }
    let member = decode_html_entities(&display_generics(trim(raw)));
    let list = if member.contains('(') { &mut info.methods } else { &mut info.attrs };
    push_capped(list, member);
}

struct ClassRelation {
    from: String,
    to: String,
    head_from: Head,
    head_to: Head,
    line: LineKind,
    label: Option<String>,
}

fn parse_class_relation(st: &str) -> Option<ClassRelation> {
    let chars: Vec<char> = st.chars().collect();
    let found = (0..chars.len()).find_map(|pos| {
        let tail: String = chars[pos..(pos + MAX_CLASS_OP).min(chars.len())].iter().collect();
        CLASS_OPS
            .iter()
            .find(|(op, ..)| {
                // `o` is also an identifier character: skip a match glued to a name.
                let glued_before = op.starts_with('o') && pos > 0 && is_id_char(chars[pos - 1]);
                let glued_after = op.ends_with('o') && chars.get(pos + op.len()).is_some_and(|&c| is_id_char(c));
                tail.starts_with(op) && !glued_before && !glued_after
            })
            .map(|op| (pos, *op))
    });
    let (pos, (op, head_from, head_to, line)) = found?;

    let lhs_raw: String = chars[..pos].iter().collect();
    let rhs_raw: String = chars[pos + op.len()..].iter().collect();
    let (lhs, card_from) = strip_cardinality_suffix(trim(&lhs_raw));
    let (rhs, card_to) = strip_cardinality_prefix(trim(&rhs_raw));

    let (to_id, rel_label) = match rhs.split_once(':') {
        Some((to_id, label)) => (trim(to_id), non_empty(decode_html_entities(trim(label)))),
        None => (trim(rhs), None),
    };
    if lhs.is_empty() || to_id.is_empty() || has_space(lhs) || has_space(to_id) {
        return None;
    }
    let parts = [card_from, rel_label.as_deref().unwrap_or(""), card_to];
    let label = parts.iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
    Some(ClassRelation {
        from: lhs.to_string(),
        to: to_id.to_string(),
        head_from,
        head_to,
        line,
        label: non_empty(label),
    })
}

/// `Class "1"`: a quoted cardinality trailing the left-hand name.
fn strip_cardinality_suffix(s: &str) -> (&str, &str) {
    let t = trim_end(s);
    if let Some(rest) = t.strip_suffix('"')
        && let Some(q) = rest.rfind('"')
    {
        return (trim_end(&rest[..q]), &rest[q + 1..]);
    }
    (t, "")
}

/// `"0..*" Class`: a quoted cardinality leading the right-hand name.
fn strip_cardinality_prefix(s: &str) -> (&str, &str) {
    let t = trim_start(s);
    if let Some(rest) = t.strip_prefix('"')
        && let Some(q) = rest.find('"')
    {
        return (trim_start(&rest[q + 1..]), &rest[..q]);
    }
    (t, "")
}

/// Mermaid writes generics as `List~T~`; show them as `List<T>`.
pub(super) fn display_generics(s: &str) -> String {
    let mut open = false;
    s.chars()
        .map(|c| {
            if c != '~' {
                return c;
            }
            open = !open;
            if open { '<' } else { '>' }
        })
        .collect()
}
