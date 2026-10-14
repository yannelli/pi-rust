//! A small DER reader for the X.509 fields `tls` reports: subject, issuer, alt names, validity,
//! serial number, and fingerprints, in the shape of Node's `getPeerCertificate()`.

use serde_json::{Map, Value, json};

/// One DER element: `(tag, content, rest)`.
fn element(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = data.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let count = (first & 0x7f) as usize;
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        (rest[..count].iter().fold(0usize, |total, byte| (total << 8) | *byte as usize), &rest[count..])
    };
    (rest.len() >= length).then(|| (tag, &rest[..length], &rest[length..]))
}

fn children(mut data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut list = Vec::new();
    while let Some((tag, content, rest)) = element(data) {
        list.push((tag, content));
        data = rest;
    }
    list
}

fn short_name(oid: &[u8]) -> Option<&'static str> {
    Some(match oid {
        [0x55, 0x04, 0x03] => "CN",
        [0x55, 0x04, 0x06] => "C",
        [0x55, 0x04, 0x07] => "L",
        [0x55, 0x04, 0x08] => "ST",
        [0x55, 0x04, 0x0a] => "O",
        [0x55, 0x04, 0x0b] => "OU",
        [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x01] => "emailAddress",
        _ => return None,
    })
}

/// A distinguished name as Node shows it: `{ CN: "x", O: "y" }`, arrays for repeated keys.
fn name(data: &[u8]) -> Value {
    let mut object = Map::new();
    for (_, set) in children(data) {
        for (_, pair) in children(set) {
            let parts = children(pair);
            let (Some((_, oid)), Some((_, text))) = (parts.first(), parts.get(1)) else { continue };
            let Some(key) = short_name(oid) else { continue };
            let text = Value::String(String::from_utf8_lossy(text).into_owned());
            match object.get_mut(key) {
                Some(Value::Array(list)) => list.push(text),
                Some(existing) => *existing = Value::Array(vec![existing.clone(), text]),
                None => {
                    object.insert(key.to_string(), text);
                }
            }
        }
    }
    Value::Object(object)
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// UTCTime or GeneralizedTime as OpenSSL prints it: `Jan  1 00:00:00 2030 GMT`.
fn time(tag: u8, data: &[u8]) -> Value {
    let text = String::from_utf8_lossy(data);
    let (year, rest) = match tag {
        0x17 if text.len() >= 12 => {
            let short: u32 = text[..2].parse().unwrap_or(0);
            (if short >= 50 { 1900 + short } else { 2000 + short }, &text[2..])
        }
        0x18 if text.len() >= 14 => (text[..4].parse().unwrap_or(0), &text[4..]),
        _ => return Value::Null,
    };
    let field = |index: usize| rest.get(index..index + 2).and_then(|part| part.parse::<u32>().ok()).unwrap_or(0);
    let month = MONTHS.get(field(0).saturating_sub(1) as usize).copied().unwrap_or("Jan");
    Value::String(format!("{month} {:>2} {:02}:{:02}:{:02} {year} GMT", field(2), field(4), field(6), field(8)))
}

fn ip_text(bytes: &[u8]) -> String {
    match bytes.len() {
        4 => std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]).to_string(),
        16 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            let segments: Vec<String> =
                octets.chunks(2).map(|pair| format!("{:X}", u16::from_be_bytes([pair[0], pair[1]]))).collect();
            segments.join(":")
        }
        _ => String::new(),
    }
}

fn alt_names(extensions: &[u8]) -> Option<String> {
    for (_, extension) in children(children(extensions).first()?.1) {
        let parts = children(extension);
        if parts.first().map(|part| part.1) != Some(&[0x55, 0x1d, 0x11][..]) {
            continue;
        }
        let (_, value) = parts.last()?;
        let names: Vec<String> = children(children(value).first()?.1)
            .into_iter()
            .filter_map(|(tag, content)| match tag {
                0x82 => Some(format!("DNS:{}", String::from_utf8_lossy(content))),
                0x87 => Some(format!("IP Address:{}", ip_text(content))),
                0x86 => Some(format!("URI:{}", String::from_utf8_lossy(content))),
                0x81 => Some(format!("email:{}", String::from_utf8_lossy(content))),
                _ => None,
            })
            .collect();
        return Some(names.join(", "));
    }
    None
}

fn colon_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect::<Vec<_>>().join(":")
}

/// The raw subject and issuer, to tell a self-signed certificate.
pub(crate) fn self_signed(der: &[u8]) -> bool {
    let Some(tbs) = tbs_fields(der) else { return false };
    let names: Vec<&[u8]> = tbs.iter().filter(|(tag, _)| *tag == 0x30).map(|(_, content)| *content).collect();
    names.len() >= 4 && names[1] == names[3]
}

fn tbs_fields(der: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    let (_, certificate, _) = element(der)?;
    let (_, tbs, _) = element(certificate)?;
    Some(children(tbs))
}

/// Node's certificate object for a DER certificate.
pub(crate) fn describe(der: &[u8]) -> Value {
    let mut result = json!({
        "raw": der.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        "fingerprint": colon_hex(ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, der).as_ref()),
        "fingerprint256": colon_hex(ring::digest::digest(&ring::digest::SHA256, der).as_ref()),
        "fingerprint512": colon_hex(ring::digest::digest(&ring::digest::SHA512, der).as_ref()),
    });
    let Some(mut fields) = tbs_fields(der) else { return result };
    if fields.first().is_some_and(|(tag, _)| *tag == 0xa0) {
        fields.remove(0);
    }
    if let [(0x02, serial), (0x30, _), (0x30, issuer), (0x30, validity), (0x30, subject), ..] = fields[..] {
        let serial = serial.strip_prefix(&[0]).filter(|rest| !rest.is_empty()).unwrap_or(serial);
        result["serialNumber"] = Value::String(serial.iter().map(|byte| format!("{byte:02X}")).collect());
        result["issuer"] = name(issuer);
        result["subject"] = name(subject);
        let times = children(validity);
        if let [(from_tag, from), (to_tag, to)] = times[..] {
            result["valid_from"] = time(from_tag, from);
            result["valid_to"] = time(to_tag, to);
        }
    }
    if let Some(names) = fields.iter().find(|(tag, _)| *tag == 0xa3).and_then(|(_, extensions)| alt_names(extensions)) {
        result["subjectaltname"] = Value::String(names);
    }
    result
}
