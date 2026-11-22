//! Resource record sections: bounds, RDLENGTH, and RDATA checks for the record types Node formats.

use std::net::{Ipv4Addr, Ipv6Addr};

use super::name::Name;
use super::{BADRESP, Kind};

/// c-ares clamps TTLs with the high bit set (RFC 2181 section 8) to `i32::MAX`.
const MAX_TTL: u32 = 0x7FFF_FFFF;

pub(super) struct Record {
    pub(super) owner: Name,
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

fn u32_at(message: &[u8], at: usize) -> Result<u32, &'static str> {
    message.get(at..at + 4).map(|bytes| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])).ok_or(BADRESP)
}

/// Parses all three record sections, so a malformed record anywhere fails the reply as it
/// does in c-ares. Bytes after the last section are ignored. Returns the answer section.
pub(super) fn sections(message: &[u8], mut at: usize, counts: [u16; 3]) -> Result<Vec<Record>, &'static str> {
    let mut answers = Vec::with_capacity(usize::from(counts[0]).min(64));
    for (section, count) in counts.into_iter().enumerate() {
        for _ in 0..count {
            let (record, next) = read(message, at)?;
            at = next;
            if section == 0 {
                answers.push(record);
            }
        }
    }
    Ok(answers)
}

fn read(message: &[u8], at: usize) -> Result<(Record, usize), &'static str> {
    let (owner, at) = Name::read(message, at)?;
    let rtype = u16_at(message, at)?;
    let class = u16_at(message, at + 2)?;
    let ttl = u32_at(message, at + 4)?.min(MAX_TTL);
    let start = at + 10;
    let end = start + usize::from(u16_at(message, at + 8)?);
    if end > message.len() {
        return Err(BADRESP);
    }
    let data = Rdata { message, at: start, end }.parse(rtype)?;
    Ok((Record { owner, class, ttl, data }, end))
}

/// Cursor over one RDATA. Fields may not run past RDLENGTH; unread trailing bytes are
/// skipped, as c-ares skips them.
struct Rdata<'a> {
    message: &'a [u8],
    at: usize,
    end: usize,
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

    fn name(&mut self) -> Result<Name, &'static str> {
        let (name, next) = Name::read(self.message, self.at)?;
        if next > self.end {
            return Err(BADRESP);
        }
        self.at = next;
        Ok(name)
    }

    fn string(&mut self) -> Result<Vec<u8>, &'static str> {
        let length = self.u8()?;
        Ok(self.bytes(usize::from(length))?.to_vec())
    }

    /// c-ares rejects NAPTR strings with bytes outside printable ASCII.
    fn printable(&mut self) -> Result<Vec<u8>, &'static str> {
        let text = self.string()?;
        if text.iter().all(|byte| (0x20..=0x7E).contains(byte)) { Ok(text) } else { Err(BADRESP) }
    }

    fn parse(mut self, rtype: u16) -> Result<Data, &'static str> {
        let Some(kind) = Kind::from_code(rtype) else { return Ok(Data::Other) };
        Ok(match kind {
            Kind::A => {
                let bytes = self.bytes(4)?;
                Data::A(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]))
            }
            Kind::Aaaa => {
                let bytes: [u8; 16] = self.bytes(16)?.try_into().map_err(|_| BADRESP)?;
                Data::Aaaa(Ipv6Addr::from(bytes))
            }
            Kind::Cname => Data::Cname(self.name()?),
            Kind::Ns => Data::Ns(self.name()?),
            Kind::Ptr => Data::Ptr(self.name()?),
            Kind::Mx => Data::Mx { priority: self.u16()?, exchange: self.name()? },
            Kind::Txt => {
                let mut chunks = vec![self.string()?];
                while self.at < self.end {
                    chunks.push(self.string()?);
                }
                Data::Txt(chunks)
            }
            Kind::Srv => {
                Data::Srv { priority: self.u16()?, weight: self.u16()?, port: self.u16()?, target: self.name()? }
            }
            Kind::Soa => Data::Soa {
                nsname: self.name()?,
                hostmaster: self.name()?,
                serial: self.u32()?,
                refresh: self.u32()?,
                retry: self.u32()?,
                expire: self.u32()?,
                minttl: self.u32()?,
            },
            Kind::Caa => {
                let critical = self.u8()?;
                let length = self.u8()?;
                if length == 0 {
                    return Err(BADRESP);
                }
                let tag = self.bytes(usize::from(length))?.to_vec();
                if self.at == self.end {
                    return Err(BADRESP);
                }
                let value = self.bytes(self.end - self.at)?.to_vec();
                Data::Caa { critical, tag, value }
            }
            Kind::Naptr => Data::Naptr {
                order: self.u16()?,
                preference: self.u16()?,
                flags: self.printable()?,
                service: self.printable()?,
                regexp: self.printable()?,
                replacement: self.name()?,
            },
            Kind::Tlsa => {
                let (usage, selector, matching) = (self.u8()?, self.u8()?, self.u8()?);
                if self.at == self.end {
                    return Err(BADRESP);
                }
                Data::Tlsa { usage, selector, matching, data: self.bytes(self.end - self.at)?.to_vec() }
            }
            Kind::Any | Kind::Reverse => Data::Other,
        })
    }
}
