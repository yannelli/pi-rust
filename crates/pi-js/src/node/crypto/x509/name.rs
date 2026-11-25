//! OpenSSL's text forms of X.509 names and general names, byte for byte as Node 22 prints them.

use super::super::der::{self, Der};
use crate::error::JsError;

/// One attribute of a distinguished name. `set` numbers the RDN that holds it.
pub(super) struct Attribute<'a> {
    pub set: usize,
    pub oid: String,
    pub tag: u8,
    pub value: &'a [u8],
    pub der: &'a [u8],
}

/// The attributes of a `Name`, from the content of its SEQUENCE.
pub(super) fn attributes(name: &[u8]) -> Result<Vec<Attribute<'_>>, JsError> {
    let mut rdns = Der::new(name);
    let mut list = Vec::new();
    let mut set = 0;
    while !rdns.is_empty() {
        let mut entries = Der::new(rdns.expect(0x31)?);
        while !entries.is_empty() {
            let mut pair = entries.seq()?;
            let oid = pair.oid()?;
            let der = pair.raw()?;
            let (tag, value) = Der::new(der).read()?;
            list.push(Attribute { set, oid, tag, value, der });
        }
        set += 1;
    }
    Ok(list)
}

/// OpenSSL's short name for an attribute type.
pub(super) fn short_name(oid: &str) -> Option<&'static str> {
    Some(match oid {
        "2.5.4.3" => "CN",
        "2.5.4.4" => "SN",
        "2.5.4.5" => "serialNumber",
        "2.5.4.6" => "C",
        "2.5.4.7" => "L",
        "2.5.4.8" => "ST",
        "2.5.4.9" => "street",
        "2.5.4.10" => "O",
        "2.5.4.11" => "OU",
        "2.5.4.12" => "title",
        "2.5.4.13" => "description",
        "2.5.4.14" => "searchGuide",
        "2.5.4.15" => "businessCategory",
        "2.5.4.16" => "postalAddress",
        "2.5.4.17" => "postalCode",
        "2.5.4.18" => "postOfficeBox",
        "2.5.4.19" => "physicalDeliveryOfficeName",
        "2.5.4.20" => "telephoneNumber",
        "2.5.4.41" => "name",
        "2.5.4.42" => "GN",
        "2.5.4.43" => "initials",
        "2.5.4.44" => "generationQualifier",
        "2.5.4.45" => "x500UniqueIdentifier",
        "2.5.4.46" => "dnQualifier",
        "2.5.4.51" => "houseIdentifier",
        "2.5.4.54" => "dmdName",
        "2.5.4.65" => "pseudonym",
        "2.5.4.72" => "role",
        "2.5.4.97" => "organizationIdentifier",
        "2.5.4.98" => "c3",
        "2.5.4.99" => "n3",
        "2.5.4.100" => "dnsName",
        "1.2.840.113549.1.9.1" => "emailAddress",
        "1.2.840.113549.1.9.2" => "unstructuredName",
        "1.2.840.113549.1.9.8" => "unstructuredAddress",
        "0.9.2342.19200300.100.1.1" => "UID",
        "0.9.2342.19200300.100.1.3" => "mail",
        "0.9.2342.19200300.100.1.25" => "DC",
        "0.9.2342.19200300.100.1.44" => "uid",
        "1.3.6.1.4.1.311.60.2.1.1" => "jurisdictionL",
        "1.3.6.1.4.1.311.60.2.1.2" => "jurisdictionST",
        "1.3.6.1.4.1.311.60.2.1.3" => "jurisdictionC",
        _ => return None,
    })
}

/// OpenSSL's long name for an access method in authority information access.
fn access_method(oid: &str) -> Option<&'static str> {
    Some(match oid.strip_prefix("1.3.6.1.5.5.7.48.")? {
        "1" => "OCSP",
        "2" => "CA Issuers",
        "3" => "AD Time Stamping",
        "4" => "ad dvcs",
        "5" => "CA Repository",
        "10" => "RPKI Manifest",
        "11" => "Signed Object",
        "13" => "RPKI Notify",
        _ => return None,
    })
}

/// Code points of a string by its type, or its bytes for UTF8String and unknown types.
fn units(tag: u8, value: &[u8]) -> Option<Vec<u32>> {
    match tag {
        30 => (value.len() % 2 == 0)
            .then(|| value.chunks(2).map(|pair| (u32::from(pair[0]) << 8) | u32::from(pair[1])).collect()),
        28 => (value.len() % 4 == 0)
            .then(|| value.chunks(4).map(|quad| u32::from_be_bytes([quad[0], quad[1], quad[2], quad[3]])).collect()),
        _ => Some(value.iter().map(|byte| u32::from(*byte)).collect()),
    }
}

/// String types OpenSSL can show as text; others are unknown.
fn is_text(tag: u8) -> bool {
    matches!(tag, 12 | 18 | 19 | 20 | 22 | 23 | 24 | 26 | 28 | 30)
}

/// `ASN1_STRING_to_UTF8`: the value as text, `None` for unknown types and bad encodings.
pub(super) fn to_utf8(tag: u8, value: &[u8]) -> Option<String> {
    if !is_text(tag) {
        return None;
    }
    if tag == 12 {
        return std::str::from_utf8(value).ok().map(str::to_string);
    }
    units(tag, value)?.into_iter().map(char::from_u32).collect()
}

const ESC_2253: u8 = 1;
const ESC_CTRL: u8 = 2;
const FIRST: u8 = 0x20;
const LAST: u8 = 0x40;

/// The escape classes of an ASCII character in OpenSSL's `char_type` table.
fn char_flags(byte: u8) -> u8 {
    match byte {
        0..=0x1f | 0x7f => ESC_CTRL,
        b' ' => FIRST | LAST,
        b'#' => FIRST,
        b'"' | b'+' | b',' | b';' | b'<' | b'>' | b'\\' => ESC_2253,
        _ => 0,
    }
}

/// `do_print_ex` with RFC 2253 escapes, UTF-8 output, and `\XX` for control characters when `ctrl`.
fn escape_value(out: &mut Vec<u8>, tag: u8, value: &[u8], ctrl: bool) -> Option<()> {
    let units = units(tag, value)?;
    let last = units.len().saturating_sub(1);
    for (index, &unit) in units.iter().enumerate() {
        if unit >= 0x80 {
            if tag == 12 {
                out.push(unit as u8);
            } else {
                out.extend_from_slice(char::from_u32(unit)?.encode_utf8(&mut [0; 4]).as_bytes());
            }
            continue;
        }
        let byte = unit as u8;
        let position = if index == last {
            LAST
        } else if index == 0 {
            FIRST
        } else {
            0
        };
        let flags = char_flags(byte) & (ESC_2253 | position | if ctrl { ESC_CTRL } else { 0 });
        if flags & (ESC_2253 | FIRST | LAST) != 0 {
            out.extend_from_slice(&[b'\\', byte]);
        } else if flags & ESC_CTRL != 0 {
            out.extend_from_slice(format!("\\{byte:02X}").as_bytes());
        } else {
            out.push(byte);
        }
    }
    Some(())
}

fn upper_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

/// `X509_NAME_print_ex`: Node's multiline form for subject and issuer, or the RFC 2253 form
/// (reversed, comma-separated, unknown values as `#` DER dumps) for a DirName.
pub(super) fn print_name(attributes: &[Attribute], rfc2253: bool) -> Option<Vec<u8>> {
    let (between, within) = if rfc2253 { (",", "+") } else { ("\n", " + ") };
    let ordered: Vec<&Attribute> =
        if rfc2253 { attributes.iter().rev().collect() } else { attributes.iter().collect() };
    let mut out = Vec::new();
    let mut previous = None;
    for attribute in ordered {
        if let Some(set) = previous {
            out.extend_from_slice(if set == attribute.set { within } else { between }.as_bytes());
        }
        previous = Some(attribute.set);
        let known = short_name(&attribute.oid);
        out.extend_from_slice(known.unwrap_or(&attribute.oid).as_bytes());
        out.push(b'=');
        if rfc2253 && (known.is_none() || !is_text(attribute.tag)) {
            out.extend_from_slice(format!("#{}", upper_hex(attribute.der)).as_bytes());
        } else {
            escape_value(&mut out, attribute.tag, attribute.value, !rfc2253)?;
        }
    }
    Some(out)
}

/// Node's `PrintAltName`: the name as is when safe, else a JSON-style quoted string.
fn alt_name(out: &mut Vec<u8>, name: &[u8], utf8: bool, prefix: Option<&str>) {
    let safe = name.iter().all(|&byte| {
        !matches!(byte, b'"' | b'\\' | b',' | b'\'')
            && if utf8 { byte >= b' ' && byte != 0x7f } else { (b' '..=b'~').contains(&byte) }
    });
    if !safe {
        out.push(b'"');
    }
    if let Some(prefix) = prefix {
        out.extend_from_slice(format!("{prefix}:").as_bytes());
    }
    if safe {
        out.extend_from_slice(name);
        return;
    }
    for &byte in name {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'"' => out.extend_from_slice(b"\\\""),
            b' '..=b'~' if byte != b',' => out.push(byte),
            0x80.. if utf8 => out.push(byte),
            _ => out.extend_from_slice(format!("\\u00{byte:02x}").as_bytes()),
        }
    }
    out.push(b'"');
}

/// The `[tag, content]` pairs of a `GeneralNames` SEQUENCE.
pub(super) fn general_names(data: &[u8]) -> Result<Vec<(u8, &[u8])>, JsError> {
    let mut names = Der::new(data).seq()?;
    let mut list = Vec::new();
    while !names.is_empty() {
        list.push(names.read()?);
    }
    Ok(list)
}

fn other_name(out: &mut Vec<u8>, content: &[u8]) -> Option<()> {
    let mut parts = Der::new(content);
    let oid = parts.oid().ok()?;
    let (tag, value) = Der::new(parts.expect(0xa0).ok()?).read().ok()?;
    let (prefix, unicode) = match oid.as_str() {
        "1.3.6.1.5.5.7.8.9" => ("SmtpUTF8Mailbox", true),
        "1.3.6.1.5.5.7.8.5" => ("XmppAddr", true),
        "1.3.6.1.5.5.7.8.7" => ("SRVName", false),
        "1.3.6.1.4.1.311.20.2.3" => ("UPN", true),
        "1.3.6.1.5.5.7.8.8" => ("NAIRealm", true),
        _ => ("", true),
    };
    out.extend_from_slice(b"othername:");
    if prefix.is_empty() || tag != if unicode { 12 } else { 22 } {
        out.extend_from_slice(b"<unsupported>");
    } else {
        alt_name(out, value, unicode, Some(prefix));
    }
    Some(())
}

/// Node's `PrintGeneralName`; `None` when the name does not decode.
fn general_name(out: &mut Vec<u8>, tag: u8, content: &[u8]) -> Option<()> {
    match tag {
        0x81 => {
            out.extend_from_slice(b"email:");
            alt_name(out, content, false, None);
        }
        0x82 => {
            out.extend_from_slice(b"DNS:");
            alt_name(out, content, false, None);
        }
        0x86 => {
            out.extend_from_slice(b"URI:");
            alt_name(out, content, false, None);
        }
        0xa4 => {
            let name = attributes(Der::new(content).expect(0x30).ok()?).ok()?;
            out.extend_from_slice(b"DirName:");
            alt_name(out, &print_name(&name, true)?, true, None);
        }
        0x87 => {
            out.extend_from_slice(b"IP Address:");
            out.extend_from_slice(ip_text(content).as_bytes());
        }
        0x88 => {
            let oid = Der::new(&der::tlv(0x06, content)).oid().ok()?;
            out.extend_from_slice(format!("Registered ID:{oid}").as_bytes());
        }
        0xa0 => other_name(out, content)?,
        0xa3 => out.extend_from_slice(b"X400Name:<unsupported>"),
        0xa5 => out.extend_from_slice(b"EdiPartyName:<unsupported>"),
        _ => return None,
    }
    Some(())
}

fn ip_text(bytes: &[u8]) -> String {
    match bytes.len() {
        4 => format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3]),
        16 => bytes
            .chunks(2)
            .map(|pair| format!("{:X}", u16::from_be_bytes([pair[0], pair[1]])))
            .collect::<Vec<_>>()
            .join(":"),
        length => format!("<invalid length={length}>"),
    }
}

/// The subjectAltName string from the extension value.
pub(super) fn print_alt_names(value: &[u8]) -> Option<String> {
    let mut out = Vec::new();
    for (index, (tag, content)) in general_names(value).ok()?.into_iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b", ");
        }
        general_name(&mut out, tag, content)?;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// The infoAccess string from an authority information access extension value.
pub(super) fn print_info_access(value: &[u8]) -> Option<String> {
    let mut descriptions = Der::new(value).seq().ok()?;
    let mut out = Vec::new();
    while !descriptions.is_empty() {
        if !out.is_empty() {
            out.push(b'\n');
        }
        let mut description = descriptions.seq().ok()?;
        let method = description.oid().ok()?;
        out.extend_from_slice(format!("{} - ", access_method(&method).unwrap_or(&method)).as_bytes());
        let (tag, content) = description.read().ok()?;
        general_name(&mut out, tag, content)?;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}
