//! The order of the reply checks, malformed input, RCODEs and classes, checked against Node
//! 22.23.5 output.

use serde_json::json;

use super::{ID, RESPONSE, ask, failed, ok, packet, record, record_raw, wire_name};
use crate::node::dns::wire::{Kind, Question, Reply};

const A: u16 = 1;
const OPT: u16 = 41;

fn a_answer() -> Vec<Vec<u8>> {
    vec![record("q.example", A, &[1, 1, 1, 1])]
}

fn question() -> Question {
    ask("q.example", Kind::A)
}

/// The UDP result, which the TCP entry point must match for every reply without TC.
fn answer_bytes(bytes: &[u8]) -> Result<Reply, &'static str> {
    let udp = question().answer(ID, bytes, false);
    if udp != Ok(Reply::Truncated) {
        assert_eq!(question().answer_tcp(ID, bytes, false), udp);
    }
    udp
}

fn resolved(measure: bool) -> Result<Reply, &'static str> {
    Ok(Reply::Answer { value: json!(["1.1.1.1"]), measure })
}

/// A reply to `q.example A` with `flags` and one A record, plus the given additional records.
fn with_additional(flags: u16, additional: &[Vec<u8>]) -> Vec<u8> {
    packet(&question(), flags, [&a_answer(), &[], additional])
}

#[test]
fn replies_to_other_queries_are_unrelated() {
    let good = packet(&question(), RESPONSE, [&a_answer(), &[], &[]]);
    let mut wrong_id = good.clone();
    wrong_id[1] ^= 1;
    assert_eq!(answer_bytes(&wrong_id), Ok(Reply::Unrelated));
    assert_eq!(answer_bytes(&[]), Ok(Reply::Unrelated));
    let other_type = ask("q.example", Kind::Aaaa);
    assert_eq!(answer_bytes(&packet(&other_type, RESPONSE, [&a_answer(), &[], &[]])), Ok(Reply::Unrelated));
    let other_name = ask("r.example", Kind::A);
    assert_eq!(answer_bytes(&packet(&other_name, RESPONSE, [&a_answer(), &[], &[]])), Ok(Reply::Unrelated));
    let mut other_class = good.clone();
    other_class[26] = 3;
    assert_eq!(answer_bytes(&other_class), Ok(Reply::Unrelated));
    let upper = ask("Q.EXAMPLE", Kind::A);
    assert_eq!(answer_bytes(&packet(&upper, RESPONSE, [&a_answer(), &[], &[]])), resolved(true));
}

#[test]
fn qr_and_known_opcodes_are_not_checked() {
    assert_eq!(answer_bytes(&with_additional(0x0100, &[])), resolved(true));
    assert_eq!(answer_bytes(&with_additional(0x8980, &[])), resolved(true));
    assert_eq!(answer_bytes(&with_additional(0x9980, &[])), Err("EBADRESP"));
}

#[test]
fn a_malformed_message_fails_before_the_id_check() {
    let mut wrong_id = question().encode(ID ^ 1);
    assert_eq!(answer_bytes(&wrong_id[..5]), Err("EBADRESP"));
    wrong_id[2] = 0x81;
    wrong_id[7] = 1;
    assert_eq!(answer_bytes(&wrong_id), Err("EBADRESP"));
    let other_class = |class: u8| {
        let mut bytes = packet(&question(), RESPONSE | 3, [&[], &[], &[]]);
        bytes[26] = class;
        answer_bytes(&bytes)
    };
    assert_eq!(other_class(2), Err("EBADRESP"));
    assert_eq!(other_class(3), Ok(Reply::Unrelated));
}

#[test]
fn truncation_is_read_after_the_sections_and_only_on_udp() {
    let clean = packet(&question(), 0x8380, [&[], &[], &[]]);
    assert_eq!(answer_bytes(&clean), Ok(Reply::Truncated));
    let mut cut = clean.clone();
    cut[7] = 1;
    cut.extend_from_slice(&[0xC0, 12, 0, 1]);
    assert_eq!(answer_bytes(&cut), Err("EBADRESP"));
    let mut wrong_id = clean.clone();
    wrong_id[1] ^= 1;
    assert_eq!(answer_bytes(&wrong_id), Ok(Reply::Unrelated));
    let tcp = |bytes: &[u8]| question().answer_tcp(ID, bytes, false);
    assert_eq!(tcp(&with_additional(0x8380, &[])), resolved(true));
    assert_eq!(tcp(&clean), failed("ENODATA"));
    assert_eq!(tcp(&wrong_id), Ok(Reply::Unrelated));
    assert_eq!(tcp(&cut), Err("EBADRESP"));
    assert_eq!(tcp(&[]), Ok(Reply::Unrelated));
}

#[test]
fn rcodes_map_like_c_ares() {
    let error = |code: &'static str, measure: bool| Ok(Reply::Error { code, measure });
    let cases = [
        (0, error("ENODATA", true)),
        (1, error("EFORMERR", false)),
        (2, error("ESERVFAIL", false)),
        (3, error("ENOTFOUND", true)),
        (4, error("ENOTIMP", false)),
        (5, error("EREFUSED", false)),
        (6, error("ENODATA", false)),
        (11, error("ENODATA", false)),
        (12, error("ESERVFAIL", false)),
        (15, error("ESERVFAIL", false)),
    ];
    for (rcode, expected) in cases {
        assert_eq!(answer_bytes(&packet(&question(), RESPONSE | rcode, [&[], &[], &[]])), expected, "rcode {rcode}");
    }
    assert_eq!(answer_bytes(&with_additional(RESPONSE | 3, &[])), error("ENOTFOUND", true));
    assert_eq!(answer_bytes(&with_additional(RESPONSE | 2, &[])), error("ESERVFAIL", false));
    assert_eq!(answer_bytes(&with_additional(RESPONSE | 6, &[])), resolved(false));
}

#[test]
fn opt_ttl_carries_the_high_rcode_bits() {
    let opt = |ttl: u32| vec![record_raw(&[0], OPT, 1232, ttl, &[])];
    assert_eq!(answer_bytes(&with_additional(RESPONSE, &opt(0))), resolved(true));
    assert_eq!(answer_bytes(&with_additional(RESPONSE, &opt(0x0100_0000))), resolved(false));
    assert_eq!(answer_bytes(&with_additional(RESPONSE | 3, &opt(0x0100_0000))), resolved(false));
    let unknown = Ok(Reply::Error { code: "ESERVFAIL", measure: false });
    assert_eq!(answer_bytes(&with_additional(RESPONSE, &opt(0x0300_0000))), unknown);
    let in_answer = [a_answer().remove(0), record_raw(&[0], OPT, 2, 0x0300_0000, &[])];
    assert_eq!(answer_bytes(&packet(&question(), RESPONSE, [&in_answer, &[], &[]])), unknown);
    let empty = packet(&question(), RESPONSE, [&[], &[], &opt(0x0100_0000)]);
    assert_eq!(answer_bytes(&empty), Ok(Reply::Error { code: "ENODATA", measure: false }));
    let two = [opt(0).remove(0), opt(0).remove(0)];
    assert_eq!(answer_bytes(&with_additional(RESPONSE, &two)), Err("EBADRESP"));
    let two_in_answer = [a_answer().remove(0), opt(0).remove(0), opt(0).remove(0)];
    assert_eq!(answer_bytes(&packet(&question(), RESPONSE, [&two_in_answer, &[], &[]])), resolved(true));
}

fn with_tail(answers: &[Vec<u8>], tail: &[u8], ancount: u16) -> Vec<u8> {
    let mut bytes = packet(&question(), RESPONSE, [answers, &[], &[]]);
    bytes[6..8].copy_from_slice(&ancount.to_be_bytes());
    bytes.extend_from_slice(tail);
    bytes
}

fn pointer_record(target: usize) -> Vec<u8> {
    record_raw(&[0xC0 | (target >> 8) as u8, target as u8], A, 1, 1, &[1, 1, 1, 1])
}

fn long_name(labels: usize, byte: u8) -> Vec<u8> {
    (0..labels).flat_map(|_| std::iter::once(63).chain([byte; 63])).chain([0]).collect()
}

#[test]
fn malformed_replies_fail_with_badresp() {
    let header_end = question().encode(ID).len();
    let bad = |bytes: &[u8]| assert_eq!(answer_bytes(bytes), Err("EBADRESP"), "{bytes:02x?}");
    bad(&question().encode(ID)[..8]);
    bad(&[1, 2, 3]);
    bad(&[0x34, 0x12, 0x81, 0x80, 0, 1, 0, 0, 0, 0, 0]);
    let mut no_question = packet(&question(), RESPONSE, [&a_answer(), &[], &[]]);
    no_question[5] = 0;
    bad(&no_question);
    let mut two_questions = packet(&question(), RESPONSE, [&a_answer(), &[], &[]]);
    two_questions[5] = 2;
    bad(&two_questions);
    bad(&with_tail(&a_answer(), &[], 2));
    bad(&with_tail(&[], &[0; 21], 2));
    bad(&with_tail(&[pointer_record(header_end + 40)], &[], 1));
    bad(&with_tail(&[pointer_record(header_end)], &[], 1));
    // A pointer must target a byte before the name that holds it.
    let own_label = [b"\x02ab".as_slice(), &[0xC0, header_end as u8]].concat();
    bad(&with_tail(&[record_raw(&own_label, A, 1, 1, &[1, 1, 1, 1])], &[], 1));
    bad(&with_tail(&[record_raw(b"\x41a\x00", A, 1, 1, &[1, 1, 1, 1])], &[], 1));
    bad(&with_tail(&[record_raw(b"\x81a\x00", A, 1, 1, &[1, 1, 1, 1])], &[], 1));
    bad(&with_tail(&[record("q.example", A, &[1, 2, 3])], &[], 1));
    bad(&with_tail(&[record("q.example", 255, &[1])], &[], 1));
    let mut overrun = with_tail(&[record("q.example", 99, &[1])], &[], 1);
    overrun.pop();
    bad(&overrun);
    let over_255 = [&long_name(4, b'a')[..256], b"\x01b\x00"].concat();
    bad(&with_tail(&[record_raw(&over_255, A, 1, 1, &[1, 1, 1, 1])], &[], 1));
    let bad_soa = [record("q.example", 6, &[1])];
    bad(&packet(&question(), RESPONSE, [&a_answer(), &bad_soa, &[]]));
    let bad_mx = [record("q.example", 15, &[1])];
    bad(&packet(&question(), RESPONSE, [&a_answer(), &[], &bad_mx]));
}

#[test]
fn tolerated_extras_match_c_ares() {
    let ok = |bytes: &[u8]| assert_eq!(answer_bytes(bytes), resolved(true), "{bytes:02x?}");
    ok(&with_tail(&a_answer(), &[1, 2, 3], 1));
    ok(&with_tail(&[record("q.example", A, &[1, 1, 1, 1, 5])], &[], 1));
    ok(&with_tail(&[pointer_record(12)], &[], 1));
    ok(&with_tail(&[record_raw(&long_name(4, b'a'), A, 1, 1, &[1, 1, 1, 1])], &[], 1));
    ok(&with_tail(&[record_raw(&wire_name("_x/y*.example"), A, 1, 1, &[1, 1, 1, 1])], &[], 1));
}

#[test]
fn record_classes_are_checked_for_parsed_types() {
    let class = |rtype: u16, class: u16, rdata: &[u8]| {
        let records = [record_raw(&wire_name("q.example"), rtype, class, 1, rdata), a_answer().remove(0)];
        answer_bytes(&packet(&question(), RESPONSE, [&records, &[], &[]]))
    };
    assert_eq!(class(A, 2, &[9, 9, 9, 9]), Err("EBADRESP"));
    assert_eq!(class(A, 255, &[9, 9, 9, 9]), Err("EBADRESP"));
    assert_eq!(class(A, 254, &[9, 9, 9, 9]), resolved(true));
    assert_eq!(class(A, 3, &[9, 9, 9, 9]), resolved(true));
    assert_eq!(class(99, 2, &[1]), resolved(true));
}

#[test]
fn other_parsed_types_are_checked_in_every_section() {
    let name = wire_name("s.example");
    let check = |rtype: u16, class: u16, rdata: &[u8]| {
        answer_bytes(&with_additional(RESPONSE, &[record_raw(&wire_name("q.example"), rtype, class, 1, rdata)]))
    };
    let (hinfo, sig, svcb, https, uri) = (13, 24, 64, 65, 256);
    assert_eq!(check(hinfo, 1, &[0, 0]), resolved(true));
    assert_eq!(check(hinfo, 1, &[]), Err("EBADRESP"));
    assert_eq!(check(hinfo, 1, &[1, 1, 0]), Err("EBADRESP"));
    assert_eq!(check(sig, 255, &[&[0; 18][..], &name, &[7]].concat()), resolved(true));
    assert_eq!(check(sig, 1, &[&[0; 18][..], &name].concat()), Err("EBADRESP"));
    assert_eq!(check(svcb, 1, &[&[0, 1][..], &name, &[0, 1, 0, 2, b'h', b'2']].concat()), resolved(true));
    assert_eq!(check(svcb, 1, &[&[0, 1][..], &name, &[0, 1, 0, 9, b'h', b'2']].concat()), Err("EBADRESP"));
    assert_eq!(check(svcb, 1, &[0, 1, 0xC0, 12]), Err("EBADRESP"));
    assert_eq!(check(https, 1, &[0, 1, 0]), resolved(true));
    assert_eq!(check(https, 1, &[0, 1, 0xC0, 12]), Err("EBADRESP"));
    assert_eq!(check(https, 1, &[&[0, 1][..], &name, &[0]].concat()), Err("EBADRESP"));
    assert_eq!(check(uri, 1, &[&[0, 1, 0, 2][..], b"http://x"].concat()), resolved(true));
    assert_eq!(check(uri, 1, &[0, 1, 0, 2]), Err("EBADRESP"));
    assert_eq!(check(uri, 1, &[0, 1, 0, 2, 1]), Err("EBADRESP"));
    assert_eq!(check(OPT, 1232, &[0, 1, 0, 9, 1]), Err("EBADRESP"));
}

#[test]
fn replies_c_ares_cannot_write_again_fail_with_formerr() {
    let a = |owner: &[u8]| record_raw(owner, A, 1, 1, &[1, 1, 1, 1]);
    let reply = |kind: Kind, records: [&[Vec<u8>]; 3], flags: u16| {
        let question = ask("q.example", kind);
        question.answer(ID, &packet(&question, flags, records), false)
    };
    let odd_owner = [a(&wire_name("my printer.example"))];
    for kind in [Kind::A, Kind::Txt, Kind::Soa, Kind::Any, Kind::Tlsa] {
        assert_eq!(reply(kind, [&a_answer(), &[], &odd_owner], RESPONSE), failed("EFORMERR"), "{kind:?}");
    }
    assert_eq!(reply(Kind::A, [&[a(&wire_name("a@b.example"))], &[], &[]], RESPONSE), failed("EFORMERR"));
    assert_eq!(reply(Kind::A, [&[a(&long_name(3, b'.'))], &[], &[]], RESPONSE), failed("EFORMERR"));
    assert_eq!(reply(Kind::A, [&[a(&long_name(4, 1))], &[], &[]], RESPONSE), failed("EFORMERR"));
    // The RCODE is mapped first.
    assert_eq!(reply(Kind::A, [&[], &odd_owner, &[]], RESPONSE | 3), failed("ENOTFOUND"));
    let long_target = [&long_name(2, 1)[..128], &wire_name("t.example")].concat();
    let cname = [record("q.example", 5, &long_target)];
    assert_eq!(reply(Kind::Cname, [&cname, &[], &[]], RESPONSE), failed("EFORMERR"));
    let srv = [record("q.example", 33, &[&[0, 1, 0, 2, 0, 3][..], &long_target].concat())];
    assert_eq!(ok(reply(Kind::Srv, [&srv, &[], &[]], RESPONSE))[0]["port"], json!(3));
    let odd_target = [record("q.example", 5, &[b"\x06a b\x01@.".as_slice(), &wire_name("t.example")].concat())];
    assert_eq!(ok(reply(Kind::Cname, [&odd_target, &[], &[]], RESPONSE)), json!(["a b\\001\\@\\..t.example"]));
}
