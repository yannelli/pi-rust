//! ER and sequence diagram grammars (upstream `parse.ts`).

use std::collections::HashMap;

use super::graph::{ClassInfo, Dir, Edge, Graph, Head, LineKind, MAX_EDGES, MAX_NODES, Shape};
use super::labels::{clean_label, decode_html_entities};
use super::parse_flow::{header_kind, statements_of};
use super::parse_state::push_capped;
use super::strings::{first_word, non_empty, strip_trailing, trim, trim_start, words};

pub(super) fn parse_er(src: &str) -> Option<(Graph, Vec<ClassInfo>)> {
    let statements = statements_of(src);
    if header_kind(&statements)? != "erdiagram" {
        return None;
    }
    let mut graph = Graph::new(Dir::Down);
    let mut infos: Vec<ClassInfo> = Vec::new();
    let mut cur_entity: Option<usize> = None;

    for st in &statements[1..] {
        if let Some(entity) = cur_entity {
            if st == "}" {
                cur_entity = None;
            } else {
                push_er_attribute(&mut infos[entity], st);
            }
            continue;
        }

        if let Some((rel, label)) = split_er_relationship(st) {
            let tokens = words(rel);
            if tokens.len() != 3 {
                return None;
            }
            let (card_l, card_r, line) = parse_er_op(tokens[1])?;
            let from = er_entity(&mut graph, &mut infos, tokens[0])?;
            let to = er_entity(&mut graph, &mut infos, tokens[2])?;
            if graph.edges.len() >= MAX_EDGES {
                return None;
            }
            let rel_label = label.map(clean_label).unwrap_or_default();
            let parts = [card_l, rel_label.as_str(), card_r];
            let label = parts.iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
            let label = non_empty(label);
            graph.edges.push(Edge { from, to, label, head_to: Head::None, head_from: Head::None, line });
            continue;
        }

        let open = st.strip_suffix('{');
        let decl = open.map_or(st.as_str(), trim);
        if decl.is_empty() || words(decl).len() != 1 {
            return None;
        }
        let idx = er_entity(&mut graph, &mut infos, decl)?;
        if open.is_some() {
            cur_entity = Some(idx);
        }
    }

    if graph.nodes.is_empty() {
        return None;
    }
    infos.resize_with(infos.len().max(graph.nodes.len()), ClassInfo::default);
    Some((graph, infos))
}

fn er_entity(graph: &mut Graph, infos: &mut Vec<ClassInfo>, token: &str) -> Option<usize> {
    let idx = match token.find('[') {
        Some(open) => {
            let id = &token[..open];
            let label = clean_label(strip_trailing(&token[open + 1..], ']'));
            if id.is_empty() || label.is_empty() {
                return None;
            }
            graph.node_label(id, label)
        }
        None => graph.node_index(token, None, Shape::Rect),
    }?;
    infos.resize_with(infos.len().max(graph.nodes.len()), ClassInfo::default);
    Some(idx)
}

fn split_er_relationship(st: &str) -> Option<(&str, Option<&str>)> {
    let (rel, label) = match st.split_once(':') {
        Some((rel, label)) => (rel, Some(trim(label))),
        None => (st, None),
    };
    words(rel).iter().any(|t| parse_er_op(t).is_some()).then_some((rel, label))
}

/// A crow's-foot operator: two cardinality glyphs around `--` or `..`.
fn parse_er_op(tok: &str) -> Option<(&'static str, &'static str, LineKind)> {
    if tok.len() != 6 || !tok.is_ascii() {
        return None;
    }
    let line = match &tok[2..4] {
        "--" => LineKind::Solid,
        ".." => LineKind::Dotted,
        _ => return None,
    };
    Some((er_card(&tok[..2])?, er_card(&tok[4..])?, line))
}

fn er_card(tok: &str) -> Option<&'static str> {
    match tok {
        "|o" | "o|" => Some("0..1"),
        "||" => Some("1"),
        "}o" | "o{" => Some("0..*"),
        "}|" | "|{" => Some("1..*"),
        _ => None,
    }
}

/// ER attributes are `type name`; a trailing quoted comment is dropped.
fn push_er_attribute(info: &mut ClassInfo, raw: &str) {
    let parts: Vec<&str> = words(raw).into_iter().take_while(|tok| !tok.starts_with('"')).collect();
    if parts.is_empty() {
        return;
    }
    push_capped(&mut info.attrs, decode_html_entities(&parts.join(" ")));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SeqHead {
    Arrow,
    Cross,
}

/// Message operators, longest first so `-->>` wins over `-->`.
const SEQ_OPS: [(&str, bool, SeqHead); 8] = [
    ("-->>", true, SeqHead::Arrow),
    ("->>", false, SeqHead::Arrow),
    ("--x", true, SeqHead::Cross),
    ("-x", false, SeqHead::Cross),
    ("--)", true, SeqHead::Arrow),
    ("-)", false, SeqHead::Arrow),
    ("-->", true, SeqHead::Arrow),
    ("->", false, SeqHead::Arrow),
];

const MAX_SEQ_OP: usize = 4;

#[derive(Clone, Copy, Debug)]
pub(super) enum NoteAnchor {
    Over { from: usize, to: usize },
    Left { at: usize },
    Right { at: usize },
}

#[derive(Clone, Debug)]
pub(super) enum SeqItem {
    Message { from: usize, to: usize, text: Option<String>, dashed: bool, head: SeqHead },
    Note { anchor: NoteAnchor, text: String },
    Divider { text: String },
}

#[derive(Default)]
pub(super) struct Sequence {
    pub labels: Vec<String>,
    pub index: HashMap<String, usize>,
    pub items: Vec<SeqItem>,
}

impl Sequence {
    fn participant(&mut self, id: &str, label: Option<String>) -> Option<usize> {
        if let Some(&existing) = self.index.get(id) {
            if let Some(label) = label {
                self.labels[existing] = label;
            }
            return Some(existing);
        }
        if self.labels.len() >= MAX_NODES {
            return None;
        }
        self.index.insert(id.to_string(), self.labels.len());
        self.labels.push(label.unwrap_or_else(|| id.to_string()));
        Some(self.labels.len() - 1)
    }

    fn push_item(&mut self, item: SeqItem) -> Option<()> {
        if self.items.len() >= MAX_EDGES {
            return None;
        }
        self.items.push(item);
        Some(())
    }
}

const IGNORED: [&str; 10] =
    ["activate", "deactivate", "create", "destroy", "title", "acctitle", "accdescr", "links", "link", "properties"];

pub(super) fn parse_sequence(src: &str) -> Option<Sequence> {
    let statements = statements_of(src);
    if header_kind(&statements)? != "sequencediagram" {
        return None;
    }
    let mut seq = Sequence::default();
    let mut autonumber = false;
    let mut msg_count = 0;
    // One entry per open block; `true` when it draws a divider on `end`.
    let mut blocks: Vec<bool> = Vec::new();

    for st in &statements[1..] {
        let first = first_word(st);
        let lower = first.to_ascii_lowercase();
        let rest = trim(&st[first.len()..]);
        match lower.as_str() {
            "participant" | "actor" => {
                if rest.is_empty() {
                    return None;
                }
                let (id, label) = match rest.split_once(" as ") {
                    Some((id, label)) => (trim(id), Some(clean_label(label))),
                    None => (rest, None),
                };
                seq.participant(id, label)?;
            }
            "autonumber" => autonumber = true,
            l if IGNORED.contains(&l) => {}
            "note" => {
                let (text, anchor) = parse_note_anchor(rest, &mut seq)?;
                seq.push_item(SeqItem::Note { anchor, text })?;
            }
            "loop" | "alt" | "opt" | "par" | "critical" | "break" | "else" | "and" | "option" => {
                if matches!(lower.as_str(), "else" | "and" | "option") {
                    // A continuation only divides a block that opened one.
                    if blocks.last() != Some(&true) {
                        continue;
                    }
                } else {
                    blocks.push(true);
                }
                seq.push_item(SeqItem::Divider { text: decode_html_entities(st) })?;
            }
            "rect" | "box" => blocks.push(false),
            "end" => {
                if blocks.pop() == Some(true) {
                    seq.push_item(SeqItem::Divider { text: "end".to_string() })?;
                }
            }
            _ => {
                let (from, to, mut text, dashed, head) = parse_seq_message(st, &mut seq)?;
                if autonumber {
                    msg_count += 1;
                    text = Some(match text {
                        Some(text) => format!("{msg_count}. {text}"),
                        None => format!("{msg_count}."),
                    });
                }
                seq.push_item(SeqItem::Message { from, to, text, dashed, head })?;
            }
        }
    }
    if seq.labels.is_empty() { None } else { Some(seq) }
}

fn parse_note_anchor(rest: &str, seq: &mut Sequence) -> Option<(String, NoteAnchor)> {
    let lower = rest.to_ascii_lowercase();
    let (kind, ids_and_text) = if lower.starts_with("over ") {
        ("over", &rest["over ".len()..])
    } else if lower.starts_with("left of ") {
        ("left", &rest["left of ".len()..])
    } else if lower.starts_with("right of ") {
        ("right", &rest["right of ".len()..])
    } else {
        return None;
    };

    let (ids, text) = ids_and_text.split_once(':')?;
    let text = decode_html_entities(trim(text));
    let parts: Vec<&str> = ids.split(',').map(trim).filter(|s| !s.is_empty()).collect();
    let a = seq.participant(parts.first()?, None)?;
    match kind {
        "left" => return Some((text, NoteAnchor::Left { at: a })),
        "right" => return Some((text, NoteAnchor::Right { at: a })),
        _ => {}
    }
    let b = match parts.get(1) {
        Some(second) => seq.participant(second, None)?,
        None => a,
    };
    Some((text, NoteAnchor::Over { from: a.min(b), to: a.max(b) }))
}

type Message = (usize, usize, Option<String>, bool, SeqHead);

fn parse_seq_message(st: &str, seq: &mut Sequence) -> Option<Message> {
    let chars: Vec<char> = st.chars().collect();
    let (pos, (op, dashed, head)) = (0..chars.len()).find_map(|pos| {
        let tail: String = chars[pos..(pos + MAX_SEQ_OP).min(chars.len())].iter().collect();
        SEQ_OPS.iter().find(|(op, ..)| tail.starts_with(op)).map(|op| (pos, *op))
    })?;

    let from_id: String = chars[..pos].iter().collect();
    let from_id = trim(&from_id);
    if from_id.is_empty() {
        return None;
    }
    // `+`/`-` activate and deactivate the target; they carry no layout meaning.
    let rest: String = chars[pos + op.len()..].iter().collect();
    let rest = trim_start(&rest).trim_start_matches(['+', '-']);

    let (to_id, text) = match rest.split_once(':') {
        Some((to_id, text)) => (trim(to_id), non_empty(decode_html_entities(trim(text)))),
        None => (trim(rest), None),
    };
    if to_id.is_empty() {
        return None;
    }
    let from = seq.participant(from_id, None)?;
    let to = seq.participant(to_id, None)?;
    Some((from, to, text, dashed, head))
}
