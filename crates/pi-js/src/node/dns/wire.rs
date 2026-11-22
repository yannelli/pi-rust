//! DNS wire codec for the `node:dns` resolve family: query encoding, response checks, and
//! record formatting in the shapes Node 22 (c-ares 1.34) returns.

mod format;
mod name;
mod record;
#[cfg(test)]
mod tests;

use serde_json::Value as Json;

use name::Name;

const BADNAME: &str = "EBADNAME";
const BADRESP: &str = "EBADRESP";
const NODATA: &str = "ENODATA";

const HEADER: usize = 12;
const CLASS_IN: u16 = 1;
const FLAG_QR: u16 = 0x8000;
const MASK_OPCODE: u16 = 0x7800;
const FLAG_TC: u16 = 0x0200;
const FLAG_RD: u16 = 0x0100;
const MASK_RCODE: u16 = 0x000F;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    A,
    Aaaa,
    Any,
    Caa,
    Cname,
    Mx,
    Naptr,
    Ns,
    Ptr,
    /// A PTR question for `reverse`; Node reports an empty answer as `ENOTFOUND`.
    Reverse,
    Soa,
    Srv,
    Tlsa,
    Txt,
}

const KINDS: [(Kind, &str, u16); 13] = [
    (Kind::A, "A", 1),
    (Kind::Ns, "NS", 2),
    (Kind::Cname, "CNAME", 5),
    (Kind::Soa, "SOA", 6),
    (Kind::Ptr, "PTR", 12),
    (Kind::Mx, "MX", 15),
    (Kind::Txt, "TXT", 16),
    (Kind::Aaaa, "AAAA", 28),
    (Kind::Srv, "SRV", 33),
    (Kind::Naptr, "NAPTR", 35),
    (Kind::Tlsa, "TLSA", 52),
    (Kind::Any, "ANY", 255),
    (Kind::Caa, "CAA", 257),
];

impl Kind {
    /// The `rrtype` names Node accepts, upper case.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        KINDS.iter().find(|entry| entry.1 == text).map(|entry| entry.0)
    }

    fn entry(self) -> (Kind, &'static str, u16) {
        let kind = if self == Kind::Reverse { Kind::Ptr } else { self };
        KINDS.into_iter().find(|entry| entry.0 == kind).unwrap_or(KINDS[0])
    }

    fn label(self) -> &'static str {
        self.entry().1
    }

    fn code(self) -> u16 {
        self.entry().2
    }

    fn from_code(code: u16) -> Option<Self> {
        KINDS.iter().find(|entry| entry.2 == code).map(|entry| entry.0)
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum Reply {
    /// Not a reply to this question: wrong ID, not a response, other opcode or question.
    Unrelated,
    /// TC is set; ask again over TCP.
    Truncated,
    Answer(Json),
    /// The answer ends in a CNAME chain without records of the asked type. Ask `name` with
    /// `Question::with_ttl_cap(ttl)`; `ttl` is the lowest CNAME TTL seen so far.
    Alias {
        name: String,
        ttl: u32,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Question {
    name: Name,
    kind: Kind,
    ttl_cap: u32,
}

impl Question {
    /// Fails with `EBADNAME` as c-ares does: empty or long labels, names over 255 wire bytes,
    /// bad escapes, and bytes outside `[A-Za-z0-9-._/*]`. IDNA conversion happens before.
    pub(crate) fn new(host: &str, kind: Kind) -> Result<Self, &'static str> {
        Ok(Self { name: Name::from_text(host)?, kind, ttl_cap: u32::MAX })
    }

    /// Limits A and AAAA TTLs in the answer, for a question that follows an `Alias`.
    pub(crate) fn with_ttl_cap(mut self, ttl: u32) -> Self {
        self.ttl_cap = self.ttl_cap.min(ttl);
        self
    }

    /// A recursive query (RD set) with one question and no EDNS record, as Node 22 sends.
    pub(crate) fn encode(&self, id: u16) -> Vec<u8> {
        let mut packet = Vec::with_capacity(HEADER + self.name.wire().len() + 4);
        for word in [id, FLAG_RD, 1, 0, 0, 0] {
            packet.extend_from_slice(&word.to_be_bytes());
        }
        packet.extend_from_slice(self.name.wire());
        packet.extend_from_slice(&self.kind.code().to_be_bytes());
        packet.extend_from_slice(&CLASS_IN.to_be_bytes());
        packet
    }

    /// Checks one response to the query sent with `id`. `Err` carries the Node error code:
    /// `EBADRESP` for a malformed reply to this question, or the code for its RCODE.
    pub(crate) fn answer(&self, id: u16, bytes: &[u8], ttl: bool) -> Result<Reply, &'static str> {
        // c-ares fails a datagram too short for a header before it looks at the ID.
        if bytes.len() < HEADER {
            return Err(BADRESP);
        }
        if u16::from_be_bytes([bytes[0], bytes[1]]) != id {
            return Ok(Reply::Unrelated);
        }
        let word = |index: usize| u16::from_be_bytes([bytes[index * 2], bytes[index * 2 + 1]]);
        let flags = word(1);
        if flags & FLAG_QR == 0 || flags & MASK_OPCODE != 0 {
            return Ok(Reply::Unrelated);
        }
        if word(2) != 1 {
            return Err(BADRESP);
        }
        let (asked, at) = Name::read(bytes, HEADER)?;
        let (qtype, qclass) = (record::u16_at(bytes, at)?, record::u16_at(bytes, at + 2)?);
        if !asked.same(&self.name) || qtype != self.kind.code() || qclass != CLASS_IN {
            return Ok(Reply::Unrelated);
        }
        // Checked before the sections: servers cut truncated replies anywhere.
        if flags & FLAG_TC != 0 {
            return Ok(Reply::Truncated);
        }
        let answers = record::sections(bytes, at + 4, [word(3), word(4), word(5)])?;
        // c-ares maps RCODEs 6 to 11 like NOERROR and 12 to 15 like SERVFAIL.
        match flags & MASK_RCODE {
            1 => Err("EFORMERR"),
            2 | 12.. => Err("ESERVFAIL"),
            3 => Err("ENOTFOUND"),
            4 => Err("ENOTIMP"),
            5 => Err("EREFUSED"),
            _ if answers.is_empty() && self.kind == Kind::Reverse => Err("ENOTFOUND"),
            _ if answers.is_empty() => Err(NODATA),
            _ => format::reply(self.kind, &self.name, &answers, ttl, self.ttl_cap),
        }
    }
}
