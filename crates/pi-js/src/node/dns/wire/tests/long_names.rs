//! SRV and NAPTR names with long escaped text, checked against Node 22.23.5 output. c-ares
//! writes the reply again for Node's parsers and splits the first 511 characters of the text.

use serde_json::json;

use super::{ID, RESPONSE, answer, ask, failed, ok, packet, record, wire_name};
use crate::node::dns::wire::Kind;

const A: u16 = 1;
const PTR: u16 = 12;
const SRV: u16 = 33;
const NAPTR: u16 = 35;
const BINARY: [u8; 63] = [1; 63];

/// The labels after the two binary labels, and the text Node keeps of them. `None` is a cut
/// inside an escape, which fails the lookup with `EFORMERR`.
const CASES: [(&[&[u8]], Option<&str>); 12] = [
    (&[b"aaaa"], Some("aaaa")),
    (&[b"aaaaa"], Some("aaaaa")),
    (&[b"aaaaab"], Some("aaaaa")),
    (&[b"a\x01b"], Some("a\\001")),
    (&[b"aaa.b"], Some("aaa\\.")),
    (&[b"aaaa", b"bb"], Some("aaaa")),
    (&[b"aaaaa", b"bb"], Some("aaaaa")),
    (&[b"aaaa\x01"], None),
    (&[b"aaa\x01"], None),
    (&[b"aa\x01"], None),
    (&[b"aaaa."], None),
    (&[&BINARY, &BINARY], None),
];

/// Two labels of 63 bytes of 0x01, which are 506 characters of text with their dots, and `tail`.
fn long_name(tail: &[&[u8]]) -> Vec<u8> {
    let mut wire = Vec::new();
    for label in [&BINARY[..], &BINARY].into_iter().chain(tail.iter().copied()) {
        wire.push(label.len() as u8);
        wire.extend_from_slice(label);
    }
    wire.push(0);
    wire
}

fn prefix() -> String {
    format!("{0}.{0}.", "\\001".repeat(63))
}

fn srv(tail: &[&[u8]]) -> Vec<u8> {
    record("q.example", SRV, &[&[0, 1, 0, 2, 0, 3][..], &long_name(tail)].concat())
}

fn naptr(tail: &[&[u8]]) -> Vec<u8> {
    record("q.example", NAPTR, &[&b"\x00\x01\x00\x02\x01s\x03sip\x00"[..], &long_name(tail)].concat())
}

#[test]
fn srv_and_naptr_names_keep_the_first_511_characters() {
    for (tail, kept) in CASES {
        let expected = kept.map(|kept| prefix() + kept);
        for (kind, record, key) in [(Kind::Srv, srv(tail), "name"), (Kind::Naptr, naptr(tail), "replacement")] {
            for asked in [kind, Kind::Any] {
                let reply = answer(asked, std::slice::from_ref(&record), false);
                match &expected {
                    Some(text) => assert_eq!(ok(reply)[0][key], json!(text), "{asked:?} {tail:?}"),
                    None => assert_eq!(reply, failed("EFORMERR"), "{asked:?} {tail:?}"),
                }
            }
        }
    }
}

#[test]
fn the_cut_text_has_node_lengths_and_object_shapes() {
    let lengths = CASES.map(|(_, kept)| kept.map(|kept| prefix().len() + kept.len()));
    let some = |length: usize| Some(length);
    assert_eq!(lengths[..7], [some(510), some(511), some(511), some(511), some(511), some(510), some(511)]);
    let name = prefix() + "aaaaa";
    assert_eq!(
        ok(answer(Kind::Srv, &[srv(&[b"aaaaab"])], false)),
        json!([{ "name": name, "port": 3, "priority": 1, "weight": 2 }])
    );
    let expected = json!([{
        "flags": "s", "service": "sip", "regexp": "", "replacement": name, "order": 1, "preference": 2, "type": "NAPTR",
    }]);
    let reply = ok(answer(Kind::Any, &[naptr(&[b"aaaaab"])], false));
    assert_eq!(reply.to_string(), expected.to_string());
}

#[test]
fn a_name_c_ares_cannot_write_again_fails_every_type_but_reverse() {
    let address = record("q.example", A, &[192, 0, 2, 1]);
    let pointer = record("q.example", PTR, &wire_name("host.test"));
    for (cut, whole) in [(srv(&[b"aaaa\x01"]), srv(&[b"aaaaab"])), (naptr(&[b"aaaa\x01"]), naptr(&[b"aaaaab"]))] {
        let failing = [address.clone(), pointer.clone(), cut];
        for kind in [Kind::A, Kind::Txt, Kind::Ptr, Kind::Any] {
            assert_eq!(answer(kind, &failing, false), failed("EFORMERR"), "{kind:?}");
        }
        assert_eq!(ok(answer(Kind::Reverse, &failing, false)), json!(["host.test"]));
        let passing = [address.clone(), whole];
        assert_eq!(ok(answer(Kind::A, &passing, false)), json!(["192.0.2.1"]));
        assert_eq!(ok(answer(Kind::Txt, &passing, false)), json!([]));
    }
}

#[test]
fn a_record_in_another_section_uses_the_same_name_copy() {
    let question = ask("q.example", Kind::A);
    let addresses = [record("q.example", A, &[192, 0, 2, 1])];
    for cut in [srv(&[b"aaaa."]), naptr(&[b"aaaa."])] {
        let records = [cut];
        for sections in [[&addresses[..], &records[..], &[]], [&addresses[..], &[], &records[..]]] {
            let bytes = packet(&question, RESPONSE, sections);
            assert_eq!(question.answer(ID, &bytes, false), failed("EFORMERR"));
        }
    }
}
