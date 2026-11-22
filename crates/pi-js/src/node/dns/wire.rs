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
const FORMERR: &str = "EFORMERR";
const NODATA: &str = "ENODATA";
const NOTFOUND: &str = "ENOTFOUND";

const HEADER: usize = 12;
const CLASS_IN: u16 = 1;
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
    /// A PTR question for `reverse`. A NOERROR reply without records is `ENOTFOUND` here, so
    /// the caller moves to its next lookup source as `ares_gethostbyaddr` does.
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
    /// A well-formed message for another query: an empty datagram, wrong ID, or other question.
    Unrelated,
    /// TC is set on a UDP reply; ask again over TCP.
    Truncated,
    /// Only NOERROR and NXDOMAIN contribute response-time samples, regardless of the API result.
    Answer { value: Json, measure: bool },
    /// The RCODE or the record parser's error for a valid reply.
    Error { code: &'static str, measure: bool },
}

#[derive(Clone, Debug)]
pub(crate) struct Question {
    name: Name,
    kind: Kind,
}

impl Question {
    /// Fails as c-ares does. `EBADNAME`: empty or long labels, over 255 bytes of labels and
    /// dots, bad escapes, bytes outside `[A-Za-z0-9-._/*]`. `EFORMERR`: over 255 characters.
    pub(crate) fn new(host: &str, kind: Kind) -> Result<Self, &'static str> {
        Ok(Self { name: Name::from_text(host)?, kind })
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

    /// Checks one UDP datagram against the query sent with `id`. `Err("EBADRESP")` is a
    /// message c-ares cannot parse; like c-ares, this check comes before the ID check.
    pub(crate) fn answer(&self, id: u16, bytes: &[u8], ttl: bool) -> Result<Reply, &'static str> {
        self.check(id, bytes, ttl, true)
    }

    /// Checks one TCP message. c-ares ignores TC on TCP.
    pub(crate) fn answer_tcp(&self, id: u16, bytes: &[u8], ttl: bool) -> Result<Reply, &'static str> {
        self.check(id, bytes, ttl, false)
    }

    /// The order of c-ares `process_answer`: parse all sections, match the ID and the
    /// question, switch to TCP on TC, then map the RCODE and run Node's parser.
    fn check(&self, id: u16, bytes: &[u8], ttl: bool, udp: bool) -> Result<Reply, &'static str> {
        if bytes.is_empty() {
            return Ok(Reply::Unrelated);
        }
        if bytes.len() < HEADER {
            return Err(BADRESP);
        }
        let word = |index: usize| u16::from_be_bytes([bytes[index * 2], bytes[index * 2 + 1]]);
        let flags = word(1);
        // c-ares accepts QUERY, IQUERY, STATUS, NOTIFY and UPDATE and does not read QR.
        if !matches!((flags & MASK_OPCODE) >> 11, 0 | 1 | 2 | 4 | 5) || word(2) != 1 {
            return Err(BADRESP);
        }
        let (asked, at) = Name::read(bytes, HEADER, true)?;
        let (qtype, qclass) = (record::u16_at(bytes, at)?, record::u16_at(bytes, at + 2)?);
        if !matches!(qclass, 1 | 3 | 4 | 254 | 255) {
            return Err(BADRESP);
        }
        let sections = record::sections(bytes, at + 4, [word(3), word(4), word(5)])?;
        if word(0) != id || !asked.same(&self.name) || qtype != self.kind.code() || qclass != CLASS_IN {
            return Ok(Reply::Unrelated);
        }
        if udp && flags & FLAG_TC != 0 {
            return Ok(Reply::Truncated);
        }
        // c-ares reads an RCODE it does not know as SERVFAIL; 6 to 11 and 16 to 23 pass on
        // to Node's parser like NOERROR, with or without records.
        let rcode = match flags & MASK_RCODE | sections.extended_rcode {
            known @ (0..=11 | 16..=23) => known,
            _ => 2,
        };
        let measure = matches!(rcode, 0 | 3);
        let code = match rcode {
            0 if sections.answers.is_empty() && self.kind == Kind::Reverse => NOTFOUND,
            0 if sections.answers.is_empty() => NODATA,
            1 => FORMERR,
            2 => "ESERVFAIL",
            3 => NOTFOUND,
            4 => "ENOTIMP",
            5 => "EREFUSED",
            // Node's parsers get no message when c-ares cannot write it again. `reverse`
            // reads the parsed reply instead.
            _ if !sections.rewritable && self.kind != Kind::Reverse => FORMERR,
            _ => match format::reply(self.kind, &asked, &sections.answers, ttl) {
                Ok(value) => return Ok(Reply::Answer { value, measure }),
                Err(code) => code,
            },
        };
        Ok(Reply::Error { code, measure })
    }
}
