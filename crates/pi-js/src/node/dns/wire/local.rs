//! Local answers for special-use DNS names (RFC 6761 and RFC 7686). The record types and TTL
//! follow hickory-resolver 0.25, which previously handled these queries for pi-js.

use serde_json::{Value as Json, json};

use super::{Kind, NODATA, NOTFOUND, Name, Question};

/// hickory's `MAX_TTL`, the TTL of each local answer.
const LOCAL_TTL: u32 = 86_400;

/// The reverse name of `::1` in wire form: `1`, 31 `0` labels, `ip6.arpa`.
const LOOPBACK_V6: [u8; 74] = {
    let mut wire = [0u8; 74];
    let mut at = 0;
    while at < 64 {
        wire[at] = 1;
        wire[at + 1] = if at == 0 { b'1' } else { b'0' };
        at += 2;
    }
    let tail = *b"\x03ip6\x04arpa\x00";
    while at < 74 {
        wire[at] = tail[at - 64];
        at += 1;
    }
    wire
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Usage {
    Normal,
    Loopback,
    NxDomain,
}

/// Zones in wire form. Length bytes are below the ASCII letters, so a comparison without
/// ASCII case covers the whole suffix.
const ZONES: [(&[u8], Usage); 5] = [
    (b"\x09localhost\x00", Usage::Loopback),
    (b"\x03127\x07in-addr\x04arpa\x00", Usage::Loopback),
    (&LOOPBACK_V6, Usage::Loopback),
    (b"\x07invalid\x00", Usage::NxDomain),
    (b"\x05onion\x00", Usage::NxDomain),
];

impl Usage {
    /// One pass over the label offsets: a zone matches where the rest of the name is the zone.
    pub(super) fn for_name(name: &Name) -> Self {
        let wire = name.wire();
        let mut at = 0;
        while let Some(&length) = wire.get(at) {
            let rest = &wire[at..];
            let zone = ZONES.iter().find(|(zone, _)| zone.len() == rest.len() && zone.eq_ignore_ascii_case(rest));
            if let Some(&(_, usage)) = zone {
                return usage;
            }
            if length == 0 {
                break;
            }
            at += 1 + usize::from(length);
        }
        Usage::Normal
    }
}

impl Question {
    /// `Err("ENOTFOUND")` for `onion` and `invalid` names, `Ok(true)` for `localhost` and the
    /// 127/8 and `::1` reverse zones, which `local_answer` answers, and `Ok(false)` otherwise.
    pub(crate) fn special_use(&self) -> Result<bool, &'static str> {
        match self.usage {
            Usage::Normal => Ok(false),
            Usage::Loopback => Ok(true),
            Usage::NxDomain => Err(NOTFOUND),
        }
    }

    /// The answer for a loopback name: 127.0.0.1, ::1 or `localhost` by record type, and
    /// `ENODATA` for other types. `None` for other names.
    pub(crate) fn local_answer(&self, ttl: bool) -> Option<Result<Json, &'static str>> {
        if self.usage != Usage::Loopback {
            return None;
        }
        let address = match self.kind {
            Kind::A => "127.0.0.1",
            Kind::Aaaa => "::1",
            Kind::Ptr | Kind::Reverse => return Some(Ok(json!(["localhost"]))),
            _ => return Some(Err(NODATA)),
        };
        Some(Ok(if ttl { json!([{ "address": address, "ttl": LOCAL_TTL }]) } else { json!([address]) }))
    }
}
