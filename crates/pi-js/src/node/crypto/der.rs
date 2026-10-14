//! Minimal DER reading and writing, plus PEM armor, for key encodings.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::error::JsError;

/// OpenSSL's error for key data it cannot decode.
pub(super) fn unsupported_key() -> JsError {
    JsError::new("error:1E08010C:DECODER routines::unsupported").with_code("ERR_OSSL_UNSUPPORTED")
}

pub(super) fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes: Vec<u8> = len.to_be_bytes().into_iter().skip_while(|byte| *byte == 0).collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend_from_slice(content);
    out
}

pub(super) fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

/// An unsigned INTEGER from big-endian bytes.
pub(super) fn uint(bytes: &[u8]) -> Vec<u8> {
    let trimmed: &[u8] = match bytes.iter().position(|byte| *byte != 0) {
        Some(start) => &bytes[start..],
        None => &[0],
    };
    let mut content = Vec::with_capacity(trimmed.len() + 1);
    if trimmed[0] & 0x80 != 0 {
        content.push(0);
    }
    content.extend_from_slice(trimmed);
    tlv(0x02, &content)
}

pub(super) fn octets(bytes: &[u8]) -> Vec<u8> {
    tlv(0x04, bytes)
}

pub(super) fn bits(bytes: &[u8]) -> Vec<u8> {
    tlv(0x03, &[&[0], bytes].concat())
}

pub(super) fn null() -> Vec<u8> {
    vec![0x05, 0x00]
}

pub(super) fn oid(dotted: &str) -> Vec<u8> {
    let arcs: Vec<u64> = dotted.split('.').map(|arc| arc.parse().unwrap_or(0)).collect();
    let mut content = vec![(arcs[0] * 40 + arcs[1]) as u8];
    for &arc in &arcs[2..] {
        let mut chunk = vec![(arc & 0x7f) as u8];
        let mut rest = arc >> 7;
        while rest > 0 {
            chunk.push(0x80 | (rest & 0x7f) as u8);
            rest >>= 7;
        }
        content.extend(chunk.iter().rev());
    }
    tlv(0x06, &content)
}

/// A cursor over a sequence of DER elements.
pub(super) struct Der<'a> {
    data: &'a [u8],
}

impl<'a> Der<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub(super) fn peek(&self) -> Option<u8> {
        self.data.first().copied()
    }

    pub(super) fn read(&mut self) -> Result<(u8, &'a [u8]), JsError> {
        let data = self.data;
        let (&tag, rest) = data.split_first().ok_or_else(unsupported_key)?;
        let (&first, rest) = rest.split_first().ok_or_else(unsupported_key)?;
        let (len, rest) = if first < 0x80 {
            (first as usize, rest)
        } else {
            let count = (first & 0x7f) as usize;
            if count == 0 || count > 4 || rest.len() < count {
                return Err(unsupported_key());
            }
            (rest[..count].iter().fold(0usize, |acc, byte| (acc << 8) | *byte as usize), &rest[count..])
        };
        if rest.len() < len {
            return Err(unsupported_key());
        }
        self.data = &rest[len..];
        Ok((tag, &rest[..len]))
    }

    pub(super) fn expect(&mut self, tag: u8) -> Result<&'a [u8], JsError> {
        match self.read()? {
            (found, content) if found == tag => Ok(content),
            _ => Err(unsupported_key()),
        }
    }

    pub(super) fn seq(&mut self) -> Result<Der<'a>, JsError> {
        self.expect(0x30).map(Der::new)
    }

    /// An unsigned INTEGER's magnitude without leading zeros.
    pub(super) fn uint(&mut self) -> Result<&'a [u8], JsError> {
        let content = self.expect(0x02)?;
        let start = content.iter().position(|byte| *byte != 0).unwrap_or(content.len());
        Ok(&content[start..])
    }

    pub(super) fn bits(&mut self) -> Result<&'a [u8], JsError> {
        match self.expect(0x03)? {
            [0, rest @ ..] => Ok(rest),
            _ => Err(unsupported_key()),
        }
    }

    pub(super) fn oid(&mut self) -> Result<String, JsError> {
        let content = self.expect(0x06)?;
        let (&first, rest) = content.split_first().ok_or_else(unsupported_key)?;
        let mut arcs = vec![(first / 40) as u64, (first % 40) as u64];
        let mut value = 0u64;
        for &byte in rest {
            value = (value << 7) | (byte & 0x7f) as u64;
            if byte & 0x80 == 0 {
                arcs.push(value);
                value = 0;
            }
        }
        Ok(arcs.iter().map(u64::to_string).collect::<Vec<_>>().join("."))
    }
}

/// The label and DER body of the first PEM block in `text`.
pub(super) fn pem_decode(text: &str) -> Result<(String, Vec<u8>), JsError> {
    let start = text.find("-----BEGIN ").ok_or_else(unsupported_key)?;
    let rest = &text[start + 11..];
    let label_end = rest.find("-----").ok_or_else(unsupported_key)?;
    let label = rest[..label_end].to_string();
    let body = &rest[label_end + 5..];
    let end = body.find(&format!("-----END {label}-----")).ok_or_else(unsupported_key)?;
    let body = &body[..end];
    if body.contains("Proc-Type:") || label == "ENCRYPTED PRIVATE KEY" {
        return Err(JsError::unsupported("crypto encrypted private keys"));
    }
    let cleaned: String = body.chars().filter(|character| !character.is_whitespace()).collect();
    let der = STANDARD.decode(cleaned).map_err(|_| unsupported_key())?;
    Ok((label, der))
}

pub(super) fn pem_encode(label: &str, der: &[u8]) -> String {
    let encoded = STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in encoded.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_der_and_pem() {
        let encoded = seq(&[uint(&[0, 0x80]), oid("1.2.840.113549.1.1.1"), null(), bits(&[1, 2])]);
        let mut outer = Der::new(&encoded);
        let mut inner = outer.seq().unwrap();
        assert_eq!(inner.uint().unwrap(), &[0x80]);
        assert_eq!(inner.oid().unwrap(), "1.2.840.113549.1.1.1");
        assert_eq!(inner.expect(0x05).unwrap(), &[] as &[u8]);
        assert_eq!(inner.bits().unwrap(), &[1, 2]);
        assert!(inner.is_empty() && outer.is_empty());
        let pem = pem_encode("PUBLIC KEY", &encoded);
        assert_eq!(pem_decode(&pem).unwrap(), ("PUBLIC KEY".to_string(), encoded));
        assert_eq!(tlv(4, &[0; 200])[..3], [4, 0x81, 200]);
    }
}
