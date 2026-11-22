//! Resource record sections: bounds, RDLENGTH, class, and RDATA checks for every record type
//! c-ares 1.34 parses.

use std::net::{Ipv4Addr, Ipv6Addr};

use super::name::Name;
use super::{BADRESP, Kind};

/// c-ares clamps TTLs with the high bit set (RFC 2181 section 8) to `i32::MAX`.
const MAX_TTL: u32 = 0x7FFF_FFFF;
/// c-ares rejects a message too short to hold its record counts at this size for each record.
const MIN_RECORD: usize = 11;

const HINFO: u16 = 13;
const SIG: u16 = 24;
const OPT: u16 = 41;
const SVCB: u16 = 64;
const HTTPS: u16 = 65;
const URI: u16 = 256;
/// Types c-ares parses without a result shape in Node.
const CHECKED: [u16; 6] = [HINFO, SIG, OPT, SVCB, HTTPS, URI];
/// Types whose RDATA names c-ares compresses when it writes the reply again.
const COMPRESSED: [Kind; 5] = [Kind::Ns, Kind::Cname, Kind::Soa, Kind::Ptr, Kind::Mx];

pub(super) struct Record {
    pub(super) class: u16,
    pub(super) ttl: u32,
    pub(super) data: Data,
}

pub(super) enum Data {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Cname(Name),
    Ns(Name),
    Ptr(Name),
    Mx { priority: u16, exchange: Name },
    Txt(Vec<Vec<u8>>),
    Srv { priority: u16, weight: u16, port: u16, target: Name },
    Soa { nsname: Name, hostmaster: Name, serial: u32, refresh: u32, retry: u32, expire: u32, minttl: u32 },
    Caa { critical: u8, tag: Vec<u8>, value: Vec<u8> },
    Naptr { order: u16, preference: u16, flags: Vec<u8>, service: Vec<u8>, regexp: Vec<u8>, replacement: Name },
    Tlsa { usage: u8, selector: u8, matching: u8, data: Vec<u8> },
    Other,
}

impl Data {
    pub(super) fn kind(&self) -> Option<Kind> {
        Some(match self {
            Data::A(_) => Kind::A,
            Data::Aaaa(_) => Kind::Aaaa,
            Data::Cname(_) => Kind::Cname,
            Data::Ns(_) => Kind::Ns,
            Data::Ptr(_) => Kind::Ptr,
            Data::Mx { .. } => Kind::Mx,
            Data::Txt(_) => Kind::Txt,
            Data::Srv { .. } => Kind::Srv,
            Data::Soa { .. } => Kind::Soa,
            Data::Caa { .. } => Kind::Caa,
            Data::Naptr { .. } => Kind::Naptr,
            Data::Tlsa { .. } => Kind::Tlsa,
            Data::Other => return None,
        })
    }
}

pub(super) fn u16_at(message: &[u8], at: usize) -> Result<u16, &'static str> {
    message.get(at..at + 2).map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]])).ok_or(BADRESP)
}

fn printable(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| (0x20..=0x7E).contains(byte))
}

fn u32_at(message: &[u8], at: usize) -> Result<u32, &'static str> {
    message.get(at..at + 4).map(|bytes| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])).ok_or(BADRESP)
}

pub(super) struct Sections {
    pub(super) answers: Vec<Record>,
    /// False when c-ares cannot write the reply again for Node's parsers, which then fail.
    pub(super) rewritable: bool,
    /// RCODE bits 4 to 11, from the TTL field of an OPT record.
    pub(super) extended_rcode: u16,
}

/// Parses all three record sections, so a malformed record anywhere fails the reply as it
/// does in c-ares. Bytes after the last section are ignored.
pub(super) fn sections(message: &[u8], mut at: usize, counts: [u16; 3]) -> Result<Sections, &'static str> {
    let total: usize = counts.into_iter().map(usize::from).sum();
    if total > message.len().saturating_sub(at) / MIN_RECORD {
        return Err(BADRESP);
    }
    let answers = Vec::with_capacity(usize::from(counts[0]).min(64));
    let mut found = Sections { answers, rewritable: true, extended_rcode: 0 };
    for (section, count) in counts.into_iter().enumerate() {
        let mut options = 0;
        for _ in 0..count {
            let (record, rtype, next) = read(message, at, &mut found)?;
            at = next;
            options += usize::from(rtype == OPT);
            // c-ares counts OPT records only in the additional section.
            if section == 2 && options > 1 {
                return Err(BADRESP);
            }
            if section == 0 {
                found.answers.push(record);
            }
        }
    }
    Ok(found)
}

/// Classes c-ares accepts on a record of a type it parses. OPT holds the UDP size there.
fn class_allowed(rtype: u16, class: u16) -> bool {
    let parsed = CHECKED.contains(&rtype) || Kind::from_code(rtype).is_some();
    !parsed || rtype == OPT || matches!(class, 1 | 3 | 4 | 254) || (rtype == SIG && class == 255)
}

fn read(message: &[u8], at: usize, found: &mut Sections) -> Result<(Record, u16, usize), &'static str> {
    let (owner, at) = Name::read(message, at, true)?;
    let rtype = u16_at(message, at)?;
    let class = u16_at(message, at + 2)?;
    let raw_ttl = u32_at(message, at + 4)?;
    let start = at + 10;
    let end = start + usize::from(u16_at(message, at + 8)?);
    if end > message.len() || !class_allowed(rtype, class) {
        return Err(BADRESP);
    }
    let mut rdata = Rdata { message, at: start, end, rewritable: true, rtype };
    let data = rdata.parse(rtype)?;
    if rtype == OPT {
        found.extended_rcode |= ((raw_ttl >> 20) & 0x0FF0) as u16;
    }
    found.rewritable &= owner.rewritable(true) && rdata.rewritable;
    Ok((Record { class, ttl: raw_ttl.min(MAX_TTL), data }, rtype, end))
}

/// Cursor over one RDATA. Fields may not run past RDLENGTH; unread trailing bytes are
/// skipped, as c-ares skips them.
struct Rdata<'a> {
    message: &'a [u8],
    at: usize,
    end: usize,
    rewritable: bool,
    rtype: u16,
}

impl<'a> Rdata<'a> {
    fn bytes(&mut self, count: usize) -> Result<&'a [u8], &'static str> {
        let next = self.at + count;
        if next > self.end {
            return Err(BADRESP);
        }
        let bytes = &self.message[self.at..next];
        self.at = next;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, &'static str> {
        let bytes = self.bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, &'static str> {
        let bytes = self.bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn name(&mut self, compression: bool) -> Result<Name, &'static str> {
        let (name, next) = Name::read(self.message, self.at, compression)?;
        if next > self.end {
            return Err(BADRESP);
        }
        self.at = next;
        self.rewritable &= if Kind::from_code(self.rtype).is_some_and(|kind| COMPRESSED.contains(&kind)) {
            name.rewritable(false)
        } else {
            name.copied_len().is_some()
        };
        Ok(name)
    }

    /// The bytes up to RDLENGTH; c-ares requires at least one.
    fn rest(&mut self) -> Result<&'a [u8], &'static str> {
        if self.at == self.end {
            return Err(BADRESP);
        }
        self.bytes(self.end - self.at)
    }

    fn string(&mut self) -> Result<Vec<u8>, &'static str> {
        let length = self.u8()?;
        Ok(self.bytes(usize::from(length))?.to_vec())
    }

    /// c-ares rejects NAPTR, HINFO and CAA tag strings with bytes outside printable ASCII.
    fn printable(&mut self) -> Result<Vec<u8>, &'static str> {
        let text = self.string()?;
        if printable(&text) { Ok(text) } else { Err(BADRESP) }
    }

    /// EDNS options and SVCB parameters: a 16-bit key, a 16-bit length, and the value.
    fn options(&mut self) -> Result<(), &'static str> {
        while self.at < self.end {
            self.u16()?;
            let length = self.u16()?;
            self.bytes(usize::from(length))?;
        }
        Ok(())
    }

    /// Checks the types c-ares parses and Node does not return. Other types pass unread.
    fn check(&mut self, rtype: u16) -> Result<(), &'static str> {
        match rtype {
            HINFO => {
                self.printable()?;
                self.printable()?;
            }
            SIG => {
                self.bytes(18)?;
                self.name(true)?;
                self.rest()?;
            }
            OPT => self.options()?,
            SVCB | HTTPS => {
                self.u16()?;
                self.name(false)?;
                self.options()?;
            }
            URI => {
                self.bytes(4)?;
                if !printable(self.rest()?) {
                    return Err(BADRESP);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn parse(&mut self, rtype: u16) -> Result<Data, &'static str> {
        let Some(kind) = Kind::from_code(rtype) else {
            self.check(rtype)?;
            return Ok(Data::Other);
        };
        Ok(match kind {
            Kind::A => {
                let bytes = self.bytes(4)?;
                Data::A(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]))
            }
            Kind::Aaaa => {
                let bytes: [u8; 16] = self.bytes(16)?.try_into().map_err(|_| BADRESP)?;
                Data::Aaaa(Ipv6Addr::from(bytes))
            }
            Kind::Cname => Data::Cname(self.name(true)?),
            Kind::Ns => Data::Ns(self.name(true)?),
            Kind::Ptr => Data::Ptr(self.name(true)?),
            Kind::Mx => Data::Mx { priority: self.u16()?, exchange: self.name(true)? },
            Kind::Txt => {
                let mut chunks = vec![self.string()?];
                while self.at < self.end {
                    chunks.push(self.string()?);
                }
                Data::Txt(chunks)
            }
            Kind::Srv => {
                Data::Srv { priority: self.u16()?, weight: self.u16()?, port: self.u16()?, target: self.name(true)? }
            }
            Kind::Soa => Data::Soa {
                nsname: self.name(true)?,
                hostmaster: self.name(true)?,
                serial: self.u32()?,
                refresh: self.u32()?,
                retry: self.u32()?,
                expire: self.u32()?,
                minttl: self.u32()?,
            },
            Kind::Caa => {
                let (critical, tag) = (self.u8()?, self.printable()?);
                if tag.is_empty() {
                    return Err(BADRESP);
                }
                Data::Caa { critical, tag, value: self.rest()?.to_vec() }
            }
            Kind::Naptr => Data::Naptr {
                order: self.u16()?,
                preference: self.u16()?,
                flags: self.printable()?,
                service: self.printable()?,
                regexp: self.printable()?,
                replacement: self.name(true)?,
            },
            Kind::Tlsa => {
                let (usage, selector, matching) = (self.u8()?, self.u8()?, self.u8()?);
                Data::Tlsa { usage, selector, matching, data: self.rest()?.to_vec() }
            }
            // c-ares fails a record of type ANY; `Reverse` has no type code.
            Kind::Any | Kind::Reverse => return Err(BADRESP),
        })
    }
}
