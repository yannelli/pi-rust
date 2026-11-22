//! Domain names: presentation text to wire labels, compressed wire to names, and the
//! escaped text c-ares gives Node.

use std::fmt::Write as _;

use super::{BADNAME, BADRESP};

const MAX_LABEL: usize = 63;
/// RFC 1035 limit on the uncompressed wire form, root byte included.
const MAX_WIRE: usize = 255;

/// Uncompressed wire form, for example `\x01q\x07example\x00`.
#[derive(Clone, Debug)]
pub(super) struct Name(Vec<u8>);

/// c-ares accepts only these bytes in query names, after escapes are decoded.
fn hostname_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'/' | b'*')
}

fn push_label(wire: &mut Vec<u8>, label: &mut Vec<u8>) -> Result<(), &'static str> {
    if label.is_empty() || wire.len() + label.len() + 2 > MAX_WIRE {
        return Err(BADNAME);
    }
    wire.push(label.len() as u8);
    wire.append(label);
    Ok(())
}

/// Decodes `\DDD` (exactly three digits, at most 255) or `\X` starting after the backslash.
fn escape(bytes: &[u8], at: usize) -> Result<(u8, usize), &'static str> {
    let first = *bytes.get(at).ok_or(BADNAME)?;
    if !first.is_ascii_digit() {
        return Ok((first, at + 1));
    }
    let digits = bytes.get(at..at + 3).filter(|digits| digits.iter().all(u8::is_ascii_digit)).ok_or(BADNAME)?;
    let value = digits.iter().fold(0u16, |value, digit| value * 10 + u16::from(digit - b'0'));
    Ok((u8::try_from(value).map_err(|_| BADNAME)?, at + 3))
}

impl Name {
    pub(super) fn wire(&self) -> &[u8] {
        &self.0
    }

    /// RFC 4343 comparison. Length bytes are at most 63, below the ASCII letters, so
    /// comparing the whole wire form without case is safe.
    pub(super) fn same(&self, other: &Name) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }

    /// Parses a query name: `""` and `"."` are the root, one trailing dot is allowed.
    pub(super) fn from_text(text: &str) -> Result<Self, &'static str> {
        let bytes = text.as_bytes();
        let mut wire = Vec::with_capacity(bytes.len().min(MAX_WIRE - 1) + 1);
        if bytes == b"." {
            wire.push(0);
            return Ok(Self(wire));
        }
        let mut label = Vec::with_capacity(MAX_LABEL);
        let mut at = 0;
        while at < bytes.len() {
            let byte = match bytes[at] {
                b'.' => {
                    push_label(&mut wire, &mut label)?;
                    at += 1;
                    continue;
                }
                b'\\' => {
                    let (byte, next) = escape(bytes, at + 1)?;
                    at = next;
                    byte
                }
                byte => {
                    at += 1;
                    byte
                }
            };
            if !hostname_byte(byte) || label.len() == MAX_LABEL {
                return Err(BADNAME);
            }
            label.push(byte);
        }
        if !label.is_empty() {
            push_label(&mut wire, &mut label)?;
        }
        wire.push(0);
        Ok(Self(wire))
    }

    /// Reads a possibly compressed name; returns it and the offset after its inline bytes.
    /// Pointers must go backward and the wire limit stops label cycles, so reading ends.
    pub(super) fn read(message: &[u8], start: usize) -> Result<(Self, usize), &'static str> {
        let mut wire = Vec::new();
        let mut at = start;
        let mut end = None;
        loop {
            let length = usize::from(*message.get(at).ok_or(BADRESP)?);
            match length & 0xC0 {
                0x00 if length == 0 => break,
                0x00 => {
                    let label = message.get(at + 1..at + 1 + length).ok_or(BADRESP)?;
                    if wire.len() + length + 2 > MAX_WIRE {
                        return Err(BADRESP);
                    }
                    wire.push(length as u8);
                    wire.extend_from_slice(label);
                    at += 1 + length;
                }
                0xC0 => {
                    let low = usize::from(*message.get(at + 1).ok_or(BADRESP)?);
                    let target = (length & 0x3F) << 8 | low;
                    if target >= at {
                        return Err(BADRESP);
                    }
                    end.get_or_insert(at + 2);
                    at = target;
                }
                _ => return Err(BADRESP),
            }
        }
        wire.push(0);
        Ok((Self(wire), end.unwrap_or(at + 1)))
    }

    /// Text without the trailing dot; the root is `""`. Reserved bytes get a backslash and
    /// bytes outside printable ASCII become `\DDD`, as c-ares writes them.
    pub(super) fn text(&self) -> String {
        let mut text = String::with_capacity(self.0.len());
        let mut at = 0;
        while let Some(&length) = self.0.get(at).filter(|length| **length != 0) {
            if at > 0 {
                text.push('.');
            }
            for &byte in &self.0[at + 1..at + 1 + usize::from(length)] {
                match byte {
                    b'"' | b'.' | b';' | b'\\' | b'(' | b')' | b'@' | b'$' => {
                        text.push('\\');
                        text.push(char::from(byte));
                    }
                    0x20..=0x7E => text.push(char::from(byte)),
                    _ => {
                        let _ = write!(text, "\\{byte:03}");
                    }
                }
            }
            at += 1 + usize::from(length);
        }
        text
    }
}
