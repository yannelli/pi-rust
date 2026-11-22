//! Query encoding and name cases, and the helpers for the reply cases in `tests/`. Expected
//! results come from Node 22.23.5 (c-ares 1.34.9) against a local loopback server.

mod aliases;
mod long_names;
mod records;
mod replies;

use super::{Kind, Question, Reply};

const ID: u16 = 0x1234;
const RESPONSE: u16 = 0x8180;

pub(super) fn wire_name(text: &str) -> Vec<u8> {
    let mut wire = Vec::new();
    for label in text.split('.').filter(|label| !label.is_empty()) {
        wire.push(label.len() as u8);
        wire.extend_from_slice(label.as_bytes());
    }
    wire.push(0);
    wire
}

pub(super) fn record_raw(owner: &[u8], rtype: u16, class: u16, ttl: u32, rdata: &[u8]) -> Vec<u8> {
    let mut record = owner.to_vec();
    record.extend_from_slice(&rtype.to_be_bytes());
    record.extend_from_slice(&class.to_be_bytes());
    record.extend_from_slice(&ttl.to_be_bytes());
    record.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    record.extend_from_slice(rdata);
    record
}

pub(super) fn record(owner: &str, rtype: u16, rdata: &[u8]) -> Vec<u8> {
    record_raw(&wire_name(owner), rtype, 1, 300, rdata)
}

/// The query for `question` turned into a response with `flags` and the given sections.
pub(super) fn packet(question: &Question, flags: u16, sections: [&[Vec<u8>]; 3]) -> Vec<u8> {
    let mut bytes = question.encode(ID);
    bytes[2..4].copy_from_slice(&flags.to_be_bytes());
    for (index, section) in sections.iter().enumerate() {
        let at = 6 + index * 2;
        bytes[at..at + 2].copy_from_slice(&(section.len() as u16).to_be_bytes());
    }
    bytes.extend(sections.iter().flat_map(|section| section.iter().flatten()));
    bytes
}

pub(super) fn ask(name: &str, kind: Kind) -> Question {
    Question::new(name, kind).expect("valid name")
}

pub(super) fn answer(kind: Kind, answers: &[Vec<u8>], ttl: bool) -> Result<Reply, &'static str> {
    let question = ask("q.example", kind);
    question.answer(ID, &packet(&question, RESPONSE, [answers, &[], &[]]), ttl)
}

/// A NOERROR or NXDOMAIN reply that fails the lookup with `code`.
pub(super) fn failed(code: &'static str) -> Result<Reply, &'static str> {
    Ok(Reply::Error { code, measure: true })
}

pub(super) fn ok(reply: Result<Reply, &'static str>) -> serde_json::Value {
    match reply {
        Ok(Reply::Answer { value, .. }) => value,
        other => panic!("expected an answer, got {other:?}"),
    }
}

#[test]
fn kinds_parse_node_names_only() {
    for name in ["A", "AAAA", "ANY", "CAA", "CNAME", "MX", "NAPTR", "NS", "PTR", "SOA", "SRV", "TLSA", "TXT"] {
        assert_eq!(Kind::parse(name).map(Kind::label), Some(name));
    }
    assert_eq!(Kind::parse("a"), None);
    assert_eq!(Kind::parse("HINFO"), None);
    assert_eq!(Kind::Caa.code(), 257);
}

#[test]
fn encodes_recursive_query_without_edns() {
    let bytes = ask("q.example", Kind::A).encode(0x97ED);
    let expected = "97ed010000010000000000000171076578616d706c650000010001";
    assert_eq!(bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>(), expected);
    assert_eq!(ask("", Kind::Ns).encode(1)[12..], [0, 0, 2, 0, 1]);
    assert_eq!(ask(".", Kind::Ns).encode(1)[12..], [0, 0, 2, 0, 1]);
}

fn wire(text: &str) -> Result<Vec<u8>, &'static str> {
    Question::new(text, Kind::A).map(|question| question.name.wire().to_vec())
}

#[test]
fn query_names_follow_c_ares_rules() {
    assert_eq!(wire("a."), Ok(wire_name("a")));
    assert_eq!(wire("a\\.b"), Ok(b"\x03a.b\x00".to_vec()));
    assert_eq!(wire("a\\065b"), Ok(b"\x03aAb\x00".to_vec()));
    assert_eq!(wire("a\\-b"), Ok(b"\x03a-b\x00".to_vec()));
    assert_eq!(wire("a\\b.test"), Ok(wire_name("ab.test")));
    assert_eq!(wire("*.a/b_c"), Ok(b"\x01*\x05a/b_c\x00".to_vec()));
    assert_eq!(wire("Q.Example"), Ok(b"\x01Q\x07Example\x00".to_vec()));
    let bad =
        ["a..b", ".a", "a..", "..", "a b", "a\\", "a\\0", "a\\25", "a\\256", "a\\(", "a\\032b", "é.x", "a\u{0}", "a:b"];
    for name in bad {
        assert_eq!(wire(name), Err("EBADNAME"), "{name:?}");
    }
    assert!(wire(&format!("{}.x", "a".repeat(63))).is_ok());
    assert_eq!(wire(&format!("{}.x", "a".repeat(64))), Err("EBADNAME"));
}

#[test]
fn query_name_length_limits_match_node() {
    let label = "a".repeat(63);
    let four = |last: usize| [&label[..], &label, &label, &"a".repeat(last)].join(".");
    // 255 bytes of labels and dots is the longest name; its wire form is 257 bytes.
    assert_eq!(wire(&four(61)).map(|wire| wire.len()), Ok(255));
    assert_eq!(wire(&four(62)).map(|wire| wire.len()), Ok(256));
    assert_eq!(wire(&four(63)).map(|wire| wire.len()), Ok(257));
    assert_eq!(wire(&format!("{}.a", four(61))).map(|wire| wire.len()), Ok(257));
    assert_eq!(wire(&format!("{}.a", four(63))), Err("EBADNAME"));
    assert_eq!(wire(&format!("{}.", four(62))).map(|wire| wire.len()), Ok(256));
    // A valid name whose text is over 255 characters fails when c-ares writes the query.
    assert_eq!(wire(&format!("{}.", four(63))), Err("EFORMERR"));
    assert_eq!(wire(&format!("{}.a.", four(61))), Err("EFORMERR"));
    assert_eq!(wire(&format!("{}.{}.test", "\\065".repeat(60), "a".repeat(20))), Err("EFORMERR"));
    assert_eq!(wire(&format!("{}.test", "\\065".repeat(64))), Err("EBADNAME"));
}
