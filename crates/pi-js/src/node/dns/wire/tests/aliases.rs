//! CNAME-only answers, owner names, classes and empty answers for each record type, checked
//! against Node 22.23.5 output. Node sends one query for each call and follows no CNAME.

use serde_json::json;

use super::{ID, RESPONSE, answer, ask, failed, ok, packet, record, record_raw, wire_name};
use crate::node::dns::wire::{Kind, Reply};

const A: u16 = 1;
const NS: u16 = 2;
const CNAME: u16 = 5;
const SOA: u16 = 6;
const PTR: u16 = 12;
const MX: u16 = 15;
const TXT: u16 = 16;
const AAAA: u16 = 28;
const TLSA: u16 = 52;
const CAA: u16 = 257;

const LISTS: [Kind; 6] = [Kind::Mx, Kind::Naptr, Kind::Srv, Kind::Tlsa, Kind::Txt, Kind::Caa];

fn mx(owner: &str, exchange: &str) -> Vec<u8> {
    record(owner, MX, &[&[0, 1][..], &wire_name(exchange)].concat())
}

fn soa(class: u16) -> Vec<u8> {
    let numbers: Vec<u8> = [1u32, 2, 3, 4, 5].iter().flat_map(|number| number.to_be_bytes()).collect();
    let data = [&wire_name("ns1.example")[..], &wire_name("host.example"), &numbers].concat();
    record_raw(&wire_name("other.example"), SOA, class, 300, &data)
}

#[test]
fn an_answer_with_only_a_cname_ends_the_lookup() {
    let only = [record("q.example", CNAME, &wire_name("target.example"))];
    for kind in [Kind::A, Kind::Aaaa].into_iter().chain(LISTS) {
        assert_eq!(ok(answer(kind, &only, false)), json!([]), "{kind:?}");
    }
    assert_eq!(ok(answer(Kind::A, &only, true)), json!([]));
    assert_eq!(ok(answer(Kind::Cname, &only, false)), json!(["target.example"]));
    assert_eq!(ok(answer(Kind::Any, &only, false)), json!([{ "value": "target.example", "type": "CNAME" }]));
    for kind in [Kind::Ns, Kind::Ptr, Kind::Reverse] {
        assert_eq!(answer(kind, &only, false), failed("ENODATA"), "{kind:?}");
    }
    assert_eq!(answer(Kind::Soa, &only, false), failed("EBADRESP"));
}

#[test]
fn an_answer_with_only_another_type_fails_per_type() {
    let txt = [record("q.example", TXT, b"\x01x")];
    for kind in [Kind::A, Kind::Aaaa, Kind::Cname, Kind::Ns, Kind::Ptr, Kind::Reverse] {
        assert_eq!(answer(kind, &txt, false), failed("ENODATA"), "{kind:?}");
    }
    assert_eq!(answer(Kind::Soa, &txt, false), failed("EBADRESP"));
    for kind in [Kind::Mx, Kind::Naptr, Kind::Srv, Kind::Tlsa, Kind::Caa] {
        assert_eq!(ok(answer(kind, &txt, false)), json!([]), "{kind:?}");
    }
    assert_eq!(ok(answer(Kind::Any, &txt, false)), json!([{ "entries": ["x"], "type": "TXT" }]));
}

#[test]
fn cname_ttl_limits_every_address_and_owners_are_not_compared() {
    let records = [
        record_raw(&wire_name("q.example"), CNAME, 1, 90, &wire_name("first.example")),
        record_raw(&wire_name("first.example"), CNAME, 1, 20, &wire_name("target.example")),
        record_raw(&wire_name("target.example"), A, 1, 60, &[192, 0, 2, 8]),
        record_raw(&wire_name("other.example"), A, 1, 80, &[192, 0, 2, 9]),
        record_raw(&wire_name("other.example"), A, 1, 5, &[192, 0, 2, 10]),
    ];
    let expected = json!([
        { "address": "192.0.2.8", "ttl": 20 }, { "address": "192.0.2.9", "ttl": 20 }, { "address": "192.0.2.10", "ttl": 5 },
    ]);
    assert_eq!(ok(answer(Kind::A, &records, true)), expected);
    assert_eq!(ok(answer(Kind::A, &records, false)), json!(["192.0.2.8", "192.0.2.9", "192.0.2.10"]));
    assert_eq!(ok(answer(Kind::Aaaa, &records, false)), json!([]));
    assert_eq!(ok(answer(Kind::Cname, &records, false)), json!(["first.example"]));
    assert_eq!(ok(answer(Kind::Any, &records, false)), json!([{ "value": "first.example", "type": "CNAME" }]));
    let reversed: Vec<Vec<u8>> = records.iter().rev().cloned().collect();
    assert_eq!(ok(answer(Kind::A, &reversed[..4], false)), json!(["192.0.2.10", "192.0.2.9", "192.0.2.8"]));
    assert_eq!(ok(answer(Kind::Cname, &reversed, false)), json!(["target.example"]));
    let high = [
        record_raw(&wire_name("q.example"), CNAME, 1, 0x9000_0000, &wire_name("t.example")),
        record_raw(&wire_name("t.example"), A, 1, u32::MAX, &[1, 1, 1, 1]),
        record_raw(&wire_name("t.example"), A, 1, 100, &[1, 1, 1, 2]),
    ];
    let expected = json!([{ "address": "1.1.1.1", "ttl": 2147483647 }, { "address": "1.1.1.2", "ttl": 100 }]);
    assert_eq!(ok(answer(Kind::A, &high, true)), expected);
}

#[test]
fn records_of_other_owners_are_returned() {
    let other = "other.example";
    assert_eq!(ok(answer(Kind::A, &[record(other, A, &[9, 9, 9, 9])], false)), json!(["9.9.9.9"]));
    assert_eq!(ok(answer(Kind::Ns, &[record(other, NS, &wire_name("ns.example"))], false)), json!(["ns.example"]));
    assert_eq!(ok(answer(Kind::Ptr, &[record(other, PTR, &wire_name("p.example"))], false)), json!(["p.example"]));
    assert_eq!(ok(answer(Kind::Reverse, &[record(other, PTR, &wire_name("p.example"))], false)), json!(["p.example"]));
    assert_eq!(
        ok(answer(Kind::Mx, &[mx(other, "mx.example")], false)),
        json!([{ "exchange": "mx.example", "priority": 1 }])
    );
    assert_eq!(ok(answer(Kind::Txt, &[record(other, TXT, b"\x01x")], false)), json!([["x"]]));
    assert_eq!(ok(answer(Kind::Soa, &[soa(1)], false))["nsname"], json!("ns1.example"));
    assert_eq!(ok(answer(Kind::Cname, &[record(other, CNAME, &wire_name("c.example"))], false)), json!(["c.example"]));
}

#[test]
fn resolve_cname_reads_the_canonical_name() {
    let a = [record("other.example", A, &[1, 1, 1, 1])];
    assert_eq!(ok(answer(Kind::Cname, &a, false)), json!(["q.example"]));
    // Node returns the question name as the reply spells it.
    let question = ask("q.example", Kind::Cname);
    let upper = packet(&ask("Q.Example", Kind::Cname), RESPONSE, [&a, &[], &[]]);
    assert_eq!(ok(question.answer(ID, &upper, false)), json!(["Q.Example"]));
    assert_eq!(answer(Kind::Cname, &[record("q.example", AAAA, &[0; 16])], false), failed("ENODATA"));
    assert_eq!(answer(Kind::Cname, &[mx("q.example", "mx.example")], false), failed("ENODATA"));
    assert_eq!(answer(Kind::A, &[record("q.example", AAAA, &[0; 16])], false), failed("ENODATA"));
}

#[test]
fn each_parser_reads_its_classes() {
    let chaos = |rtype: u16, rdata: &[u8]| record_raw(&wire_name("q.example"), rtype, 3, 300, rdata);
    assert_eq!(answer(Kind::A, &[chaos(A, &[1, 1, 1, 1])], false), failed("ENODATA"));
    let txt = [chaos(TXT, b"\x05chaos"), record_raw(&wire_name("q.example"), TXT, 4, 300, b"\x06hesiod")];
    assert_eq!(ok(answer(Kind::Txt, &txt, false)), json!([["chaos"]]));
    let caa = [chaos(CAA, &[&[128, 5][..], b"issue", b"ab"].concat())];
    assert_eq!(ok(answer(Kind::Caa, &caa, false)), json!([{ "critical": 128, "issue": "ab" }]));
    let exchange = [&[0, 1][..], &wire_name("mx.example")].concat();
    assert_eq!(ok(answer(Kind::Mx, &[chaos(MX, &exchange)], false)), json!([]));
    assert_eq!(answer(Kind::Ptr, &[chaos(PTR, &wire_name("p.example"))], false), failed("ENODATA"));
    let tlsa = [record("q.example", CNAME, &wire_name("t.example")), chaos(TLSA, &[1, 2, 3, 0xAB])];
    let expected = json!([null, { "certUsage": 1, "selector": 2, "match": 3, "data": [171] }]);
    assert_eq!(ok(answer(Kind::Tlsa, &tlsa, false)), expected);
    assert_eq!(answer(Kind::Soa, &[soa(3)], false), failed("EBADRESP"));
    assert_eq!(ok(answer(Kind::Any, &[soa(3)], false))[0]["type"], json!("SOA"));
}

#[test]
fn rcodes_passed_to_the_parsers_fail_an_empty_answer_per_type() {
    let reply = |kind: Kind| {
        let question = ask("q.example", kind);
        question.answer(ID, &packet(&question, RESPONSE | 6, [&[], &[], &[]]), false)
    };
    let nodata = [Kind::A, Kind::Aaaa, Kind::Cname, Kind::Mx, Kind::Txt, Kind::Ns, Kind::Ptr, Kind::Srv, Kind::Naptr];
    for kind in nodata.into_iter().chain([Kind::Caa, Kind::Reverse]) {
        assert_eq!(reply(kind), Ok(Reply::Error { code: "ENODATA", measure: false }), "{kind:?}");
    }
    assert_eq!(reply(Kind::Soa), Ok(Reply::Error { code: "EBADRESP", measure: false }));
    assert_eq!(ok(reply(Kind::Tlsa)), json!([]));
    assert_eq!(ok(reply(Kind::Any)), json!([]));
}

#[test]
fn reverse_reads_ptr_records() {
    let question = ask("37.2.0.192.in-addr.arpa", Kind::Reverse);
    assert_eq!(question.encode(1)[question.encode(1).len() - 4..], [0, 12, 0, 1]);
    let reply = |records: &[Vec<u8>]| question.answer(ID, &packet(&question, RESPONSE, [records, &[], &[]]), false);
    // `ENOTFOUND` moves the caller to its next lookup source; `ENODATA` ends the lookup.
    assert_eq!(reply(&[]), failed("ENOTFOUND"));
    assert_eq!(reply(&[record("37.2.0.192.in-addr.arpa", TXT, b"\x01x")]), failed("ENODATA"));
    let ptr = [
        record("37.2.0.192.in-addr.arpa", PTR, &wire_name("host.example")),
        record("a@b.example", PTR, &wire_name("other.example")),
        record_raw(&wire_name("37.2.0.192.in-addr.arpa"), PTR, 3, 300, &wire_name("chaos.example")),
    ];
    assert_eq!(ok(reply(&ptr)), json!(["host.example", "other.example"]));
    assert_eq!(answer(Kind::Ptr, &[], false), failed("ENODATA"));
    assert_eq!(answer(Kind::Ptr, &ptr, false), failed("EFORMERR"));
}
