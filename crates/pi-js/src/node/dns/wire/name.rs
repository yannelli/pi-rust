//! Domain names: presentation text to wire labels, compressed wire to names, and the
//! escaped text c-ares gives Node.

use std::fmt::Write as _;

use super::{BADNAME, BADRESP, FORMERR};

const MAX_LABEL: usize = 63;
/// c-ares limit on the labels plus the dots between them; the wire form is two bytes longer.
const MAX_PRESENTATION: usize = 255;
/// c-ares fails a query (`EFORMERR`) or a reply it writes again when a name's text is longer.
const MAX_TEXT: usize = 255;
/// c-ares copies this much of a name's text before it splits the labels.
const MAX_COPY: usize = 511;
const MAX_POINTERS: usize = 128;

/// Uncompressed wire form, for example `\x01q\x07example\x00`.
#[derive(Clone, Debug)]
pub(super) struct Name(Vec<u8>);

/// c-ares accepts only these bytes in query names, after escapes are decoded.
fn hostname_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'/' | b'*')
}

fn push_label(wire: &mut Vec<u8>, label: &mut Vec<u8>) -> Result<(), &'static str> {
    if label.is_empty() || wire.len() + label.len() > MAX_PRESENTATION {
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

/// Characters c-ares writes for one label byte: the byte, a backslash and the byte, or `\DDD`.
fn width(byte: u8) -> usize {
    match byte {
        b'"' | b'.' | b';' | b'\\' | b'(' | b')' | b'@' | b'$' => 2,
        0x20..=0x7E => 1,
        _ => 4,
    }
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

    fn labels(&self) -> impl Iterator<Item = &[u8]> {
        let mut rest = &self.0[..];
        std::iter::from_fn(move || {
            let (&length, tail) = rest.split_first()?;
            let label = tail.get(..usize::from(length)).filter(|label| !label.is_empty())?;
            rest = &tail[label.len()..];
            Some(label)
        })
    }

    /// Parses a query name: `""` and `"."` are the root, one trailing dot is allowed. Fails
    /// with `EBADNAME`, or with `EFORMERR` when the name is valid and its text is too long.
    pub(super) fn from_text(text: &str) -> Result<Self, &'static str> {
        let bytes = &text.as_bytes()[..text.len().min(MAX_COPY)];
        let mut wire = Vec::with_capacity(bytes.len().min(MAX_PRESENTATION) + 2);
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
        if text.len() > MAX_TEXT {
            return Err(FORMERR);
        }
        Ok(Self(wire))
    }

    /// Reads a name; returns it and the offset after its inline bytes. As in c-ares, a
    /// pointer must target a byte before every byte this name has read, so reading ends.
    pub(super) fn read(message: &[u8], start: usize, compression: bool) -> Result<(Self, usize), &'static str> {
        let mut wire = Vec::new();
        let (mut at, mut lowest) = (start, start);
        let mut end = None;
        let mut pointers = 0;
        loop {
            lowest = lowest.min(at);
            let length = usize::from(*message.get(at).ok_or(BADRESP)?);
            match length & 0xC0 {
                0x00 if length == 0 => break,
                0x00 => {
                    let label = message.get(at + 1..at + 1 + length).ok_or(BADRESP)?;
                    if wire.len() + length > MAX_PRESENTATION {
                        return Err(BADRESP);
                    }
                    wire.push(length as u8);
                    wire.extend_from_slice(label);
                    at += 1 + length;
                }
                0xC0 if compression => {
                    let low = usize::from(*message.get(at + 1).ok_or(BADRESP)?);
                    let target = (length & 0x3F) << 8 | low;
                    pointers += 1;
                    if target >= lowest || pointers > MAX_POINTERS {
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

    /// Whether c-ares can write this name again when it hands the reply to Node's parsers.
    /// Owner names must also hold only hostname bytes there.
    pub(super) fn rewritable(&self, owner: bool) -> bool {
        let text: usize = self.labels().map(|label| label.iter().copied().map(width).sum::<usize>() + 1).sum();
        text <= MAX_TEXT + 1 && (!owner || self.labels().flatten().copied().all(hostname_byte))
    }

    /// Length after c-ares copies up to 511 text characters. A cut inside an escape is invalid.
    pub(super) fn copied_len(&self) -> Option<usize> {
        let mut end = 0;
        'labels: for (index, label) in self.labels().enumerate() {
            if index > 0 {
                // A dot that ends the copy leaves no label after it.
                if end + 1 >= MAX_COPY {
                    break;
                }
                end += 1;
            }
            for &byte in label {
                if end == MAX_COPY {
                    break 'labels;
                }
                end += width(byte);
                if end > MAX_COPY {
                    return None;
                }
            }
        }
        Some(end)
    }

    pub(super) fn copied_text(&self) -> Option<String> {
        let mut text = self.text();
        if text.len() > MAX_COPY {
            text.truncate(self.copied_len()?);
        }
        Some(text)
    }

    /// Text without the trailing dot; the root is `""`. Reserved bytes get a backslash and
    /// bytes outside printable ASCII become `\DDD`, as c-ares writes them.
    pub(super) fn text(&self) -> String {
        let mut text = String::with_capacity(self.0.len());
        for (index, label) in self.labels().enumerate() {
            if index > 0 {
                text.push('.');
            }
            for &byte in label {
                match width(byte) {
                    1 => text.push(char::from(byte)),
                    2 => text.extend(['\\', char::from(byte)]),
                    _ => {
                        let _ = write!(text, "\\{byte:03}");
                    }
                }
            }
        }
        text
    }
}
