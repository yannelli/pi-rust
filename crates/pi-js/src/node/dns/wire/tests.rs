//! Header, question, name and malformed-input cases. Expected results come from Node 22.23.5
//! (c-ares 1.34.9) against a local fake server, except where a case says pi differs.

mod records;

use super::{Kind, Question, Reply};

const ID: u16 = 0x1234;
const RESPONSE: u16 = 0x8180;
const A: u16 = 1;

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

fn a_answer() -> Vec<Vec<u8>> {
    vec![record("q.example", A, &[1, 1, 1, 1])]
}

fn answer_bytes(bytes: &[u8]) -> Result<Reply, &'static str> {
    ask("q.example", Kind::A).answer(ID, bytes, false)
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

#[test]
fn query_names_follow_c_ares_rules() {
    let wire = |text: &str| Question::new(text, Kind::A).map(|question| question.name.wire().to_vec());
    assert_eq!(wire("a."), Ok(wire_name("a")));
    assert_eq!(wire("a\\.b"), Ok(b"\x03a.b\x00".to_vec()));
    assert_eq!(wire("a\\065b"), Ok(b"\x03aAb\x00".to_vec()));
    assert_eq!(wire("a\\-b"), Ok(b"\x03a-b\x00".to_vec()));
    assert_eq!(wire("*.a/b_c"), Ok(b"\x01*\x05a/b_c\x00".to_vec()));
    assert_eq!(wire("Q.Example"), Ok(b"\x01Q\x07Example\x00".to_vec()));
    for bad in ["a..b", ".a", "a..", "..", "a b", "a\\", "a\\0", "a\\25", "a\\256", "a\\(", "a\\032b", "é.x", "a\u{0}"]
    {
        assert_eq!(wire(bad), Err("EBADNAME"), "{bad:?}");
    }
    assert!(wire(&format!("{}.x", "a".repeat(63))).is_ok());
    assert_eq!(wire(&format!("{}.x", "a".repeat(64))), Err("EBADNAME"));
    let longest = [&"a".repeat(63)[..], &"a".repeat(63), &"a".repeat(63), &"a".repeat(61)].join(".");
    assert_eq!(wire(&longest).map(|wire| wire.len()), Ok(255));
    // Node 22 also sends 254- and 255-character names (wire 256-257 bytes); pi keeps RFC 1035.
    assert_eq!(wire(&format!("{longest}a")), Err("EBADNAME"));
}

#[test]
fn unrelated_replies_are_ignored() {
    let question = ask("q.example", Kind::A);
    let good = packet(&question, RESPONSE, [&a_answer(), &[], &[]]);
    let mut wrong_id = good.clone();
    wrong_id[1] ^= 1;
    assert_eq!(answer_bytes(&wrong_id), Ok(Reply::Unrelated));
    // Node 22 accepts QR=0 and other opcodes; pi treats them as not a reply.
    assert_eq!(answer_bytes(&packet(&question, 0x0180, [&a_answer(), &[], &[]])), Ok(Reply::Unrelated));
    assert_eq!(answer_bytes(&packet(&question, 0x9180, [&a_answer(), &[], &[]])), Ok(Reply::Unrelated));
    let other_type = ask("q.example", Kind::Aaaa);
    assert_eq!(answer_bytes(&packet(&other_type, RESPONSE, [&a_answer(), &[], &[]])), Ok(Reply::Unrelated));
    let other_name = ask("r.example", Kind::A);
    assert_eq!(answer_bytes(&packet(&other_name, RESPONSE, [&a_answer(), &[], &[]])), Ok(Reply::Unrelated));
    let mut other_class = good.clone();
    other_class[26] = 3;
    assert_eq!(answer_bytes(&other_class), Ok(Reply::Unrelated));
    let upper = ask("Q.EXAMPLE", Kind::A);
    assert_eq!(
        answer_bytes(&packet(&upper, RESPONSE, [&a_answer(), &[], &[]])),
        Ok(Reply::Answer(serde_json::json!(["1.1.1.1"])))
    );
}

#[test]
fn truncated_is_reported_before_the_sections() {
    // pi differs: Node 22 parses the sections first and fails this reply with EBADRESP.
    let question = ask("q.example", Kind::A);
    let mut bytes = packet(&question, 0x8380, [&[], &[], &[]]);
    bytes[7] = 3;
    assert_eq!(answer_bytes(&bytes), Ok(Reply::Truncated));
}

#[test]
fn rcodes_map_like_c_ares() {
    let question = ask("q.example", Kind::A);
    let cases = [
        (0, Err("ENODATA")),
        (1, Err("EFORMERR")),
        (2, Err("ESERVFAIL")),
        (3, Err("ENOTFOUND")),
        (4, Err("ENOTIMP")),
        (5, Err("EREFUSED")),
        (6, Err("ENODATA")),
        (11, Err("ENODATA")),
        (12, Err("ESERVFAIL")),
        (15, Err("ESERVFAIL")),
    ];
    for (rcode, expected) in cases {
        assert_eq!(answer_bytes(&packet(&question, RESPONSE | rcode, [&[], &[], &[]])), expected, "rcode {rcode}");
    }
    assert_eq!(answer_bytes(&packet(&question, RESPONSE | 3, [&a_answer(), &[], &[]])), Err("ENOTFOUND"));
    assert_eq!(answer_bytes(&packet(&question, RESPONSE | 2, [&a_answer(), &[], &[]])), Err("ESERVFAIL"));
    let rcode_6 = answer_bytes(&packet(&question, RESPONSE | 6, [&a_answer(), &[], &[]]));
    assert_eq!(rcode_6, Ok(Reply::Answer(serde_json::json!(["1.1.1.1"]))));
}

fn with_tail(answers: &[Vec<u8>], tail: &[u8], ancount: u16) -> Vec<u8> {
    let mut bytes = packet(&ask("q.example", Kind::A), RESPONSE, [answers, &[], &[]]);
    bytes[6..8].copy_from_slice(&ancount.to_be_bytes());
    bytes.extend_from_slice(tail);
    bytes
}

fn pointer_record(target: usize) -> Vec<u8> {
    record_raw(&[0xC0 | (target >> 8) as u8, target as u8], A, 1, 1, &[1, 1, 1, 1])
}

#[test]
fn malformed_replies_fail_with_badresp() {
    let question = ask("q.example", Kind::A);
    let header_end = question.encode(ID).len();
    let bad = |bytes: &[u8]| assert_eq!(answer_bytes(bytes), Err("EBADRESP"), "{bytes:02x?}");
    bad(&question.encode(ID)[..8]);
    bad(&[]);
    bad(&[1, 2, 3]);
    bad(&[0x34, 0x12, 0x81, 0x80, 0, 1, 0, 0, 0, 0, 0]);
    let mut no_question = packet(&question, RESPONSE, [&a_answer(), &[], &[]]);
    no_question[5] = 0;
    bad(&no_question);
    let mut two_questions = packet(&question, RESPONSE, [&a_answer(), &[], &[]]);
    two_questions[5] = 2;
    bad(&two_questions);
    bad(&with_tail(&a_answer(), &[], 2));
    bad(&with_tail(&[pointer_record(header_end + 40)], &[], 1));
    bad(&with_tail(&[pointer_record(header_end)], &[], 1));
    bad(&with_tail(&[record_raw(b"\x41a\x00", A, 1, 1, &[1, 1, 1, 1])], &[], 1));
    bad(&with_tail(&[record_raw(b"\x81a\x00", A, 1, 1, &[1, 1, 1, 1])], &[], 1));
    bad(&with_tail(&[record("q.example", A, &[1, 2, 3])], &[], 1));
    let mut overrun = with_tail(&[record("q.example", 99, &[1])], &[], 1);
    overrun.pop();
    bad(&overrun);
    let long: Vec<u8> = (0..4).flat_map(|_| std::iter::once(63).chain([b'a'; 63])).chain([0]).collect();
    bad(&with_tail(&[record_raw(&long, A, 1, 1, &[1, 1, 1, 1])], &[], 1));
    let bad_soa = [record("q.example", 6, &[1])];
    bad(&packet(&question, RESPONSE, [&a_answer(), &bad_soa, &[]]));
    let bad_mx = [record("q.example", 15, &[1])];
    bad(&packet(&question, RESPONSE, [&a_answer(), &[], &bad_mx]));
}

#[test]
fn tolerated_extras_match_c_ares() {
    let ok = |bytes: &[u8]| assert_eq!(answer_bytes(bytes), Ok(Reply::Answer(serde_json::json!(["1.1.1.1"]))));
    ok(&with_tail(&a_answer(), &[1, 2, 3], 1));
    ok(&with_tail(&[record("q.example", A, &[1, 1, 1, 1, 5])], &[], 1));
    ok(&with_tail(&[pointer_record(12)], &[], 1));
    let opt = [record_raw(&[0], 41, 1232, 0, &[])];
    ok(&packet(&ask("q.example", Kind::A), RESPONSE, [&a_answer(), &[], &opt]));
}
