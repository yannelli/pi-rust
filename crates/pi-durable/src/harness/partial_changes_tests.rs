//! Port of upstream `test/harness-partial-changes.test.ts`. Partial commits assigned the growing strings of a
//! streamed answer, and Chord compared each one whole (upstream #9265).

use pi_ai::{AssistantMessage, ContentBlock, Message, ToolCall, ToolCallArgumentsStream};
use pi_chord::delta::{Op, Tracker, parse_ops, track};
use pi_chord::json;
use serde_json::Value;

use super::*;

/// A response streaming one text block (started empty) and then one tool call, as a provider makes it: the partial
/// message it changes and the events it emits. Partial commits go into a tracked document, as the generation's do.
struct Stream {
    partial: AssistantMessage,
    changes: PartialChanges,
    tracker: Tracker,
    args: ToolCallArgumentsStream,
    ops: Vec<Vec<Op>>,
}

fn tool_call(arguments: pi_ai::JsonObject) -> ContentBlock {
    let call = ToolCall {
        id: "c1".into(),
        name: "write".into(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    };
    ContentBlock::ToolCall(call)
}

impl Stream {
    fn new() -> Self {
        let partial = AssistantMessage { content: vec![ContentBlock::text("")], ..AssistantMessage::default() };
        let tracker = track(json!({ "generation": { "attempt": 1 } }));
        let (changes, args) = (PartialChanges::default(), ToolCallArgumentsStream::new());
        Self { partial, changes, tracker, args, ops: Vec::new() }
    }

    fn text(&mut self, delta: &str) {
        if let ContentBlock::Text { text, .. } = &mut self.partial.content[0] {
            text.push_str(delta);
        }
        let partial = self.partial.clone();
        self.changes.observe(&AssistantMessageEvent::TextDelta { content_index: 0, delta: delta.into(), partial });
    }

    fn argument(&mut self, delta: &str) {
        if self.partial.content.len() < 2 {
            self.partial.content.push(tool_call(pi_ai::JsonObject::new()));
            let partial = self.partial.clone();
            self.changes.observe(&AssistantMessageEvent::ToolcallStart { content_index: 1, partial });
        }
        let arguments = self.args.append(delta).clone();
        self.partial.content[1] = tool_call(arguments);
        let partial = self.partial.clone();
        self.changes.observe(&AssistantMessageEvent::ToolcallDelta { content_index: 1, delta: delta.into(), partial });
    }

    /// The provider ends both blocks.
    fn end_blocks(&mut self) {
        let content = self.partial.content[0].as_text().unwrap_or_default().to_owned();
        let partial = self.partial.clone();
        self.changes.observe(&AssistantMessageEvent::TextEnd { content_index: 0, content, partial });
        let ContentBlock::ToolCall(tool_call) = self.partial.content[1].clone() else { panic!("a tool call") };
        let partial = self.partial.clone();
        self.changes.observe(&AssistantMessageEvent::ToolcallEnd { content_index: 1, tool_call, partial });
    }

    /// One partial commit, as the generation's throttle makes it; `fail` aborts it like a rejected commit.
    fn commit(&mut self, fail: bool) {
        let encoded = serde_json::to_value(Message::Assistant(self.partial.clone())).unwrap();
        let mut message = Json::from(encoded);
        let taken = self.changes.take(&mut message);
        let mut change = self.tracker.begin_change();
        self.changes.commit(&mut change, &[Seg::key("generation")], message, &taken).unwrap();
        if fail {
            change.abort();
            self.changes.restore(&taken);
            return;
        }
        let prepared = change.prepare().unwrap();
        self.ops.push(prepared.ops.clone());
        self.tracker.adopt(&prepared).unwrap();
    }

    /// The committed text and arguments.
    fn committed(&self) -> (Json, Json) {
        let content = &self.tracker.value()["generation"]["message"]["content"];
        (content[0]["text"].clone(), content[1]["arguments"].clone())
    }
}

/// `text` in pieces of `size` characters.
fn pieces(text: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(size).map(|piece| piece.iter().collect()).collect()
}

fn arguments() -> Value {
    serde_json::json!({
        "path": "src/a.ts",
        "n": 1234,
        "ok": true,
        "content": "line \"one\"\n\tline \\ two é 😀\n",
        "edits": [{ "oldText": "a", "newText": "b\nc" }, { "oldText": "", "newText": "x".repeat(50) }],
        "nested": { "deep": { "list": [1, [2, "three"], { "four": null }] } },
    })
}

#[test]
fn commits_tool_call_arguments_equal_to_the_final_parse_at_every_piece_size_and_commit_rhythm() {
    let text = serde_json::to_string(&arguments()).unwrap();
    for size in [1, 2, 3, 5, 7, 16] {
        for every in [1, 3, 10] {
            let mut stream = Stream::new();
            for (index, piece) in pieces(&text, size).iter().enumerate() {
                stream.argument(piece);
                if index % every == every - 1 {
                    stream.commit(false);
                }
            }
            stream.commit(false);
            assert_eq!(stream.committed().1, Json::from(arguments()), "size {size}, every {every}");
        }
    }
}

#[test]
fn handles_a_number_ending_and_a_string_starting_in_one_piece_and_escapes_split_across_pieces() {
    let mut stream = Stream::new();
    let pieces = [
        r#"{"n":1"#,
        r#"2,"s":"a"#,
        r"b\",
        r#"u00e9\""#,
        r"q\u",
        "00",
        "41",
        r#"","t":tr"#,
        "ue,",
        r#""u":"\ud83d"#,
        r#"\ude00"}"#,
    ];
    for piece in pieces {
        stream.argument(piece);
        stream.commit(false);
    }
    assert_eq!(stream.committed().1, json!({ "n": 12, "s": "abé\"qA", "t": true, "u": "😀" }));
}

#[test]
fn appends_growing_argument_and_text_strings_instead_of_assigning_them() {
    let mut stream = Stream::new();
    stream.text("Hello");
    stream.argument(r#"{"path":"a.ts","content":"x"#);
    stream.commit(false);
    for _ in 0..20 {
        stream.text(" more");
        stream.argument("yz");
        stream.commit(false);
    }
    let appends = json!([
        ["a", ["generation", "message", "content", 0, "text"], " more"],
        ["a", ["generation", "message", "content", 1, "arguments", "content"], "yz"],
    ]);
    let appends = parse_ops(&appends).unwrap();
    for ops in &stream.ops[1..] {
        assert_eq!(*ops, appends);
    }
    let text = format!("Hello{}", " more".repeat(20));
    let content = format!("x{}", "yz".repeat(20));
    assert_eq!(stream.committed(), (Json::from(text), json!({ "path": "a.ts", "content": content })));
}

#[test]
fn applies_the_changes_of_a_failed_commit_in_the_next_one() {
    let mut stream = Stream::new();
    stream.argument(r#"{"a":"x"#);
    stream.commit(false);
    stream.argument(r#"y","b":[1"#);
    stream.commit(true);
    stream.argument(r#",2],"c":"z"}"#);
    stream.commit(false);
    assert_eq!(stream.committed().1, json!({ "a": "xy", "b": [1, 2], "c": "z" }));
}

#[test]
fn applies_a_failed_commit_s_changes_again_even_when_it_would_have_added_the_call() {
    let mut stream = Stream::new();
    stream.commit(false);
    stream.argument(r#"{"a":"x"#);
    stream.commit(true);
    stream.argument(r#"y""#);
    stream.commit(false);
    assert_eq!(stream.committed().1, json!({ "a": "xy" }));
}

#[test]
fn falls_back_to_the_snapshot_when_the_committed_message_lost_a_call_whose_changes_were_committed() {
    let mut stream = Stream::new();
    stream.argument(r#"{"a":"x"#);
    stream.commit(false);
    // Another writer resets the generation's partial.
    let mut change = stream.tracker.begin_change();
    change.state().unwrap().set("generation", json!({ "attempt": 1 })).unwrap();
    let prepared = change.prepare().unwrap();
    stream.tracker.adopt(&prepared).unwrap();
    stream.argument(r#"y""#);
    stream.commit(false);
    assert_eq!(stream.committed().1, json!({ "a": "xy" }));
    stream.argument(r#","b":2}"#);
    stream.commit(false);
    assert_eq!(stream.committed().1, json!({ "a": "xy", "b": 2 }));
}

#[test]
fn commits_the_final_arguments_after_the_call_ends_and_drops_text_appends_when_a_block_ends() {
    let mut stream = Stream::new();
    stream.text("draft");
    stream.argument(r#"{"a":"x"#);
    stream.commit(false);
    // Providers may replace a block's text and arguments as it ends; the last commit assigns what they ended with.
    stream.text(" more");
    stream.partial.content[0] = ContentBlock::text("replaced");
    let serde_json::Value::Object(last) = serde_json::json!({ "a": "final" }) else { panic!("an object") };
    stream.partial.content[1] = tool_call(last);
    stream.end_blocks();
    stream.commit(false);
    assert_eq!(stream.committed(), (Json::from("replaced"), json!({ "a": "final" })));
}
