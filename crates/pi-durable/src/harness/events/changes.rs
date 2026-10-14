//! Message changes and tool updates derived from the view operations of one publication (spec section 9.4).

use pi_chord::delta::{Op, Seg};
use pi_chord::{Json, JsonMap};

const PARTIAL_PATH: [&str; 4] = ["docs", "pi.live", "generation", "message"];

pub(super) fn object<'a>(fields: impl IntoIterator<Item = (&'a str, Json)>) -> Json {
    Json::object(fields.into_iter().collect::<JsonMap>())
}

/// Whether `path` starts with the keys and indexes of `prefix`.
fn starts_with(path: &[Seg], prefix: &[Seg]) -> bool {
    prefix.len() <= path.len() && prefix.iter().zip(path).all(|(left, right)| left == right)
}

fn keys(keys: &[&str]) -> Vec<Seg> {
    keys.iter().map(|key| Seg::key(*key)).collect()
}

fn segment_json(segment: &Seg) -> Json {
    match segment {
        Seg::Key(key) => Json::from(key.clone()),
        Seg::Index(index) => Json::from(*index),
    }
}

fn whole(message: &Json) -> Vec<Json> {
    vec![object([("type", Json::from("message")), ("message", message.clone())])]
}

/// Translate the view operations on the in-flight message into message changes.
pub(super) fn message_changes(view_ops: &[Op], message: &Json) -> Vec<Json> {
    let partial = keys(&PARTIAL_PATH);
    let mut changes = Vec::new();
    // A block sent whole already holds every later change to it in this batch.
    let mut sent_whole: Vec<u64> = Vec::new();
    for op in view_ops {
        // View operations never replace the root; a replacement reads as the whole message.
        let path = op.path();
        if !starts_with(path, &partial) {
            if starts_with(&partial, path) {
                return whole(message);
            }
            continue;
        }
        let rest = &path[partial.len()..];
        let field = |index: usize| match rest.get(index) {
            Some(Seg::Key(key)) => Some(&**key),
            _ => None,
        };
        if field(0) == Some("usage") {
            continue;
        }
        if field(0) != Some("content") {
            return whole(message);
        }
        if rest.len() == 1 {
            let Op::Splice(_, index, 0, blocks) = op else { return whole(message) };
            for (offset, block) in blocks.iter().enumerate() {
                let kind = match block["type"].as_str() {
                    Some("text") => "text_start",
                    Some("thinking") => "thinking_start",
                    _ => "toolcall_start",
                };
                let content_index = Json::from(index + offset as u64);
                changes.push(object([
                    ("type", Json::from(kind)),
                    ("contentIndex", content_index),
                    ("block", block.clone()),
                ]));
            }
            continue;
        }
        let Some(Seg::Index(content_index)) = rest.get(1) else { return whole(message) };
        let content_index = *content_index;
        if sent_whole.contains(&content_index) {
            continue;
        }
        let at = ("contentIndex", Json::from(content_index));
        match (op, field(2)) {
            (Op::Append(_, delta), Some(kind @ ("text" | "thinking"))) if rest.len() == 3 => {
                let kind = if kind == "text" { "text_delta" } else { "thinking_delta" };
                changes.push(object([("type", Json::from(kind)), at, ("delta", Json::from(delta.clone()))]));
            }
            (Op::Append(_, delta), Some("arguments")) => {
                let path = Json::array(rest[3..].iter().map(segment_json).collect());
                let fields =
                    [("type", Json::from("toolcall_delta")), at, ("path", path), ("delta", Json::from(delta.clone()))];
                changes.push(object(fields));
            }
            _ => {
                sent_whole.push(content_index);
                let block = message["content"][content_index as usize].clone();
                changes.push(object([("type", Json::from("block")), at, ("block", block)]));
            }
        }
    }
    changes
}

/// The identifying fields of a slot's tool events.
pub(super) fn call_of(slot: &Json) -> Vec<(&'static str, Json)> {
    let mut fields = vec![("toolCallId", slot["callId"].clone()), ("toolName", slot["name"].clone())];
    if let Some(task_id) = slot.get("taskId") {
        fields.push(("taskId", task_id.clone()));
    }
    if let Some(parent) = slot.get("parentCallId") {
        fields.push(("parentToolCallId", parent.clone()));
        fields.push(("parentTaskId", slot["parentTaskId"].clone()));
    }
    fields
}

/// Whether two optional values are the same revision, as `===` compares them upstream.
pub(super) fn same(left: Option<&Json>, right: Option<&Json>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.same_ref(right),
        (None, None) => true,
        _ => false,
    }
}

/// Output, details, and diagnostics changes of a running slot at `at` in `pi.live`, from the view operations on it.
pub(super) fn tool_update(
    view_ops: &[Op],
    at: (&str, usize),
    slot: &Json,
    previous: &Json,
) -> Option<Vec<(&'static str, Json)>> {
    let mut output_path = keys(&["docs", "pi.live", at.0]);
    output_path.extend([Seg::Index(at.1 as u64), Seg::key("output")]);
    let (mut trim_start, mut append, mut set) = (0u64, String::new(), false);
    for op in view_ops {
        if !starts_with(op.path(), &output_path) {
            continue;
        }
        match op {
            Op::Truncate(_, count) => trim_start += count,
            Op::Append(_, text) => append.push_str(text),
            _ => set = true,
        }
    }
    let mut update = Vec::new();
    let changed = !same(slot.get("output"), previous.get("output"));
    if set || (changed && trim_start == 0 && append.is_empty()) {
        let text = slot.get("output").cloned().unwrap_or_else(|| Json::from(""));
        update.push(("output", object([("set", text)])));
    } else if trim_start > 0 || !append.is_empty() {
        let mut output = Vec::new();
        if trim_start > 0 {
            output.push(("trimStart", Json::from(trim_start)));
        }
        if !append.is_empty() {
            output.push(("append", Json::from(append)));
        }
        update.push(("output", object(output)));
    }
    // A safe replay clears a running slot's progress: removed details send `null`, removed diagnostics `[]`.
    if !same(slot.get("details"), previous.get("details")) {
        update.push(("details", slot.get("details").cloned().unwrap_or(Json::Null)));
    }
    if !same(slot.get("diagnostics"), previous.get("diagnostics")) {
        update.push(("diagnostics", slot.get("diagnostics").cloned().unwrap_or_else(|| Json::array(Vec::new()))));
    }
    (!update.is_empty()).then_some(update)
}
