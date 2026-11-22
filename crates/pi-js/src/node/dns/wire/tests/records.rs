//! Record shapes, strings and `resolveAny` order, checked against Node 22.23.5 output.

use serde_json::json;

use super::{answer, ok, record, record_raw, wire_name};
use crate::node::dns::wire::Kind;

const A: u16 = 1;
const NS: u16 = 2;
const CNAME: u16 = 5;
const SOA: u16 = 6;
const PTR: u16 = 12;
const MX: u16 = 15;
const TXT: u16 = 16;
const AAAA: u16 = 28;
const SRV: u16 = 33;
const NAPTR: u16 = 35;
const TLSA: u16 = 52;
const CAA: u16 = 257;

fn text(parts: &[&[u8]]) -> Vec<u8> {
    parts.iter().flat_map(|part| std::iter::once(part.len() as u8).chain(part.iter().copied())).collect()
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn soa_data(minttl: u32) -> Vec<u8> {
    let numbers: Vec<u8> = [1u32, 2, 3, 4, minttl].iter().flat_map(|number| number.to_be_bytes()).collect();
    cat(&[&wire_name("ns1.example"), &wire_name("host.example"), &numbers])
}

fn every_type() -> Vec<Vec<u8>> {
    let v6 = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    vec![
        record("q.example", TLSA, &[3, 1, 1, 0xAB]),
        record("q.example", TXT, &text(&[b"a", b"b"])),
        record("q.example", CAA, &cat(&[&[128, 5], b"issue", b"ca.example"])),
        record("q.example", SOA, &soa_data(5)),
        record(
            "q.example",
            NAPTR,
            &cat(&[&[0, 10, 0, 20], &text(&[b"s", b"SIP+D2U", b""]), &wire_name("_sip._udp.example")]),
        ),
        record("q.example", PTR, &wire_name("p.example")),
        record("q.example", SRV, &cat(&[&[0, 1, 0, 2, 0x13, 0xC4], &wire_name("t.example")])),
        record("q.example", NS, &wire_name("ns.example")),
        record("q.example", MX, &cat(&[&[0, 10], &wire_name("mx.example")])),
        record_raw(&wire_name("q.example"), AAAA, 1, 77, &v6),
        record_raw(&wire_name("q.example"), A, 1, 66, &[1, 1, 1, 1]),
        record("q.example", 99, &[1, 2]),
    ]
}

#[test]
fn addresses_with_and_without_ttl() {
    let records = [
        record_raw(&wire_name("q.example"), A, 1, 50, &[1, 2, 3, 4]),
        record_raw(&wire_name("q.example"), A, 1, u32::MAX, &[1, 2, 3, 5]),
        record_raw(&wire_name("q.example"), A, 3, 1, &[9, 9, 9, 9]),
    ];
    assert_eq!(ok(answer(Kind::A, &records, false)), json!(["1.2.3.4", "1.2.3.5"]));
    let expected = json!([{ "address": "1.2.3.4", "ttl": 50 }, { "address": "1.2.3.5", "ttl": 2147483647 }]);
    assert_eq!(ok(answer(Kind::A, &records, true)), expected);
    let v6 = |octets: [u8; 16]| record("q.example", AAAA, &octets);
    let mut compat = [0; 16];
    compat[12..].copy_from_slice(&[1, 2, 3, 4]);
    let mut mapped = compat;
    mapped[10..12].copy_from_slice(&[0xFF, 0xFF]);
    let mut loopback = [0; 16];
    loopback[15] = 1;
    let found = ok(answer(Kind::Aaaa, &[v6(compat), v6(mapped), v6(loopback), v6([0; 16])], false));
    assert_eq!(found, json!(["::1.2.3.4", "::ffff:1.2.3.4", "::1", "::"]));
}

#[test]
fn every_record_shape() {
    let all = every_type();
    assert_eq!(ok(answer(Kind::Mx, &all, false)), json!([{ "exchange": "mx.example", "priority": 10 }]));
    assert_eq!(ok(answer(Kind::Ns, &all, false)), json!(["ns.example"]));
    assert_eq!(ok(answer(Kind::Ptr, &all, false)), json!(["p.example"]));
    assert_eq!(ok(answer(Kind::Txt, &all, false)), json!([["a", "b"]]));
    let srv = json!([{ "name": "t.example", "port": 5060, "priority": 1, "weight": 2 }]);
    assert_eq!(ok(answer(Kind::Srv, &all, false)), srv);
    let naptr = json!([{ "flags": "s", "service": "SIP+D2U", "regexp": "", "replacement": "_sip._udp.example", "order": 10, "preference": 20 }]);
    assert_eq!(ok(answer(Kind::Naptr, &all, false)), naptr);
    let soa = json!({ "nsname": "ns1.example", "hostmaster": "host.example", "serial": 1, "refresh": 2, "retry": 3, "expire": 4, "minttl": 5 });
    assert_eq!(ok(answer(Kind::Soa, &all, false)), soa);
    assert_eq!(ok(answer(Kind::Caa, &all, false)), json!([{ "critical": 128, "issue": "ca.example" }]));
    let aaaa = json!([{ "address": "2001:db8::1", "ttl": 77 }]);
    assert_eq!(ok(answer(Kind::Aaaa, &all, true)), aaaa);
}

#[test]
fn any_lists_types_in_node_order() {
    let expected = r#"[{"address":"1.1.1.1","ttl":66,"type":"A"},{"address":"2001:db8::1","ttl":77,"type":"AAAA"},{"exchange":"mx.example","priority":10,"type":"MX"},{"value":"ns.example","type":"NS"},{"entries":["a","b"],"type":"TXT"},{"name":"t.example","port":5060,"priority":1,"weight":2,"type":"SRV"},{"value":"p.example","type":"PTR"},{"flags":"s","service":"SIP+D2U","regexp":"","replacement":"_sip._udp.example","order":10,"preference":20,"type":"NAPTR"},{"nsname":"ns1.example","hostmaster":"host.example","serial":1,"refresh":2,"retry":3,"expire":4,"minttl":5,"type":"SOA"},{"certUsage":3,"selector":1,"match":1,"data":[171]},{"critical":128,"issue":"ca.example","type":"CAA"}]"#;
    assert_eq!(ok(answer(Kind::Any, &every_type(), false)).to_string(), expected);
    let with_cname = [
        record("q.example", CNAME, &wire_name("c.example")),
        record("c.example", A, &[1, 1, 1, 1]),
        record("q.example", MX, &cat(&[&[0, 1], &wire_name("mx.example")])),
    ];
    let expected =
        json!([{ "value": "c.example", "type": "CNAME" }, { "exchange": "mx.example", "priority": 1, "type": "MX" }]);
    assert_eq!(ok(answer(Kind::Any, &with_cname, false)), expected);
    let two_soa = [record("q.example", SOA, &soa_data(5)), record("q.example", SOA, &soa_data(6))];
    assert_eq!(ok(answer(Kind::Any, &two_soa, false)).as_array().map(Vec::len), Some(1));
    assert_eq!(ok(answer(Kind::Any, &[record("q.example", 99, &[1])], false)), json!([]));
}

#[test]
fn any_drops_a_records_when_the_answer_has_a_cname() {
    let v6 = [1; 16];
    let records = [
        record_raw(&wire_name("q.example"), A, 1, 50, &[1, 1, 1, 1]),
        record_raw(&wire_name("q.example"), CNAME, 1, 7, &wire_name("t.example")),
        record_raw(&wire_name("q.example"), AAAA, 1, 90, &v6),
        record("q.example", MX, &cat(&[&[0, 1], &wire_name("mx.example")])),
        record("q.example", TLSA, &[1, 2, 3, 0xAB]),
        record("q.example", SOA, &soa_data(5)),
    ];
    let expected = r#"[{"value":"t.example","type":"CNAME"},{"address":"101:101:101:101:101:101:101:101","ttl":7,"type":"AAAA"},{"exchange":"mx.example","priority":1,"type":"MX"},{"nsname":"ns1.example","hostmaster":"host.example","serial":1,"refresh":2,"retry":3,"expire":4,"minttl":5,"type":"SOA"},null,null,null,null,{"certUsage":1,"selector":2,"match":3,"data":[171]}]"#;
    assert_eq!(ok(answer(Kind::Any, &records, false)).to_string(), expected);
    assert_eq!(
        ok(answer(Kind::Aaaa, &records, true)),
        json!([{ "address": "101:101:101:101:101:101:101:101", "ttl": 7 }])
    );
}

#[test]
fn soa_numbers_and_caa_values_follow_node_conversions() {
    let numbers: Vec<u8> = [u32::MAX, 0x8000_0001, u32::MAX, 0x9000_0000, u32::MAX]
        .iter()
        .flat_map(|number| number.to_be_bytes())
        .collect();
    let data = cat(&[&wire_name("ns1.example"), &wire_name("host.example"), &numbers]);
    let expected = r#"{"nsname":"ns1.example","hostmaster":"host.example","serial":4294967295,"refresh":-2147483647,"retry":-1,"expire":-1879048192,"minttl":4294967295}"#;
    assert_eq!(ok(answer(Kind::Soa, &[record("q.example", SOA, &data)], false)).to_string(), expected);
    let caa = [record("q.example", CAA, &cat(&[&[0, 5], b"issue", b"a\x00b"]))];
    assert_eq!(ok(answer(Kind::Caa, &caa, false)), json!([{ "critical": 0, "issue": "a" }]));
}

#[test]
fn strings_are_latin1_and_names_escaped() {
    let txt = [record("q.example", TXT, &text(&[b"a", b"b"])), record("q.example", TXT, &text(&[b"", b"\xE9\x00z"]))];
    assert_eq!(ok(answer(Kind::Txt, &txt, false)), json!([["a", "b"], ["", "\u{e9}\u{0}z"]]));
    let caa = [
        record("q.example", CAA, &cat(&[&[1, 5], b"issue", b"c\xE9"])),
        record("q.example", CAA, &cat(&[&[0, 3], b"i-x", b"v"])),
    ];
    assert_eq!(
        ok(answer(Kind::Caa, &caa, false)),
        json!([{ "critical": 1, "issue": "c\u{e9}" }, { "critical": 0, "i-x": "v" }])
    );
    let odd = cat(&[b"\x03a.b\x04\\ \x7F\xE9\x02\"$\x00"]);
    assert_eq!(ok(answer(Kind::Ns, &[record("q.example", NS, &odd)], false)), json!(["a\\.b.\\\\ \\127\\233.\\\"\\$"]));
    let root = [record("q.example", MX, &[0, 0, 0])];
    assert_eq!(ok(answer(Kind::Mx, &root, false)), json!([{ "exchange": "", "priority": 0 }]));
    let compressed = cat(&[&[0xC0, 12, 1, b'h', 0xC0, 14], &soa_data(u32::MAX)[27..]]);
    let soa = ok(answer(Kind::Soa, &[record("q.example", SOA, &compressed)], false));
    assert_eq!(
        (soa["nsname"].clone(), soa["hostmaster"].clone(), soa["minttl"].clone()),
        (json!("q.example"), json!("h.example"), json!(4294967295u32))
    );
}

#[test]
fn malformed_rdata_fails() {
    let bad = |kind: Kind, rtype: u16, rdata: &[u8]| {
        assert_eq!(answer(kind, &[record("q.example", rtype, rdata)], false), Err("EBADRESP"), "{rdata:02x?}");
    };
    bad(Kind::Txt, TXT, &[]);
    bad(Kind::Txt, TXT, &[5, b'a']);
    bad(Kind::Caa, CAA, &[0, 0]);
    bad(Kind::Caa, CAA, &[0, 9, b'a']);
    bad(Kind::Caa, CAA, &cat(&[&[0, 5], b"issue"]));
    bad(Kind::Caa, CAA, &cat(&[&[0, 5], b"is\x01ue", b"ab"]));
    bad(Kind::Naptr, NAPTR, &cat(&[&[0, 1, 0, 2], &text(&[b"\xE9", b"x", b"y"]), &[0]]));
    bad(Kind::Naptr, NAPTR, &cat(&[&[0, 1, 0, 2], &text(&[b"a\x01", b"b", b"c"]), &[0]]));
    bad(Kind::Mx, MX, &[0, 1, 3, b'a']);
    bad(Kind::Soa, SOA, &soa_data(1)[..40]);
    bad(Kind::Aaaa, AAAA, &[0; 15]);
    let naptr = cat(&[&[0, 1, 0, 2], &text(&[b"a", b"a b", b"!^.*$!x!"]), &[0]]);
    let expected = json!([{ "flags": "a", "service": "a b", "regexp": "!^.*$!x!", "replacement": "", "order": 1, "preference": 2 }]);
    assert_eq!(ok(answer(Kind::Naptr, &[record("q.example", NAPTR, &naptr)], false)), expected);
}

#[test]
fn tlsa_keeps_node_answer_index_slots() {
    let records = [
        record("q.example", CNAME, &wire_name("c.example")),
        record("c.example", TLSA, &[1, 2, 3, 4, 5]),
        record("c.example", A, &[1, 1, 1, 1]),
        record("other.example", TLSA, &[3, 1, 1, 0xAB]),
    ];
    let expected = json!([null, { "certUsage": 1, "selector": 2, "match": 3, "data": [4, 5] }, null, { "certUsage": 3, "selector": 1, "match": 1, "data": [171] }]);
    assert_eq!(ok(answer(Kind::Tlsa, &records, false)), expected);
    let any = json!([{ "value": "c.example", "type": "CNAME" }, null, { "certUsage": 1, "selector": 2, "match": 3, "data": [4, 5] }, null, { "certUsage": 3, "selector": 1, "match": 1, "data": [171] }]);
    assert_eq!(ok(answer(Kind::Any, &records, false)), any);
    assert_eq!(answer(Kind::Tlsa, &[record("q.example", TLSA, &[3, 1, 1])], false), Err("EBADRESP"));
    assert_eq!(answer(Kind::Tlsa, &[record("q.example", TLSA, &[3, 1])], false), Err("EBADRESP"));
}
