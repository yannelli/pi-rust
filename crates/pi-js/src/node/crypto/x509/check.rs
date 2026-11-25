//! `X509_check_host`, `X509_check_email`, and `X509_check_ip_asc` as in OpenSSL 3.5.

use super::super::der::Der;
use super::name::{Attribute, to_utf8};
use crate::error::JsError;

const ALWAYS_CHECK_SUBJECT: u32 = 0x1;
const NO_WILDCARDS: u32 = 0x2;
const NO_PARTIAL_WILDCARDS: u32 = 0x4;
const MULTI_LABEL_WILDCARDS: u32 = 0x8;
const SINGLE_LABEL_SUBDOMAINS: u32 = 0x10;
const NEVER_CHECK_SUBJECT: u32 = 0x20;
const DOT_SUBDOMAINS: u32 = 0x8000;
const IA5: u8 = 22;
const OCTETS: u8 = 4;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Kind {
    Host,
    Email,
    Ip,
}

type Equal = fn(&[u8], &[u8], u32) -> bool;

fn skip_prefix(pattern: &[u8], subject_len: usize, flags: u32) -> &[u8] {
    if flags & DOT_SUBDOMAINS == 0 {
        return pattern;
    }
    let mut rest = pattern;
    while rest.len() > subject_len && rest[0] != 0 {
        if flags & SINGLE_LABEL_SUBDOMAINS != 0 && rest[0] == b'.' {
            break;
        }
        rest = &rest[1..];
    }
    if rest.len() == subject_len { rest } else { pattern }
}

fn equal_nocase(pattern: &[u8], subject: &[u8], flags: u32) -> bool {
    let pattern = skip_prefix(pattern, subject.len(), flags);
    pattern.len() == subject.len() && pattern.iter().zip(subject).all(|(&l, r)| l != 0 && l.eq_ignore_ascii_case(r))
}

fn equal_case(pattern: &[u8], subject: &[u8], flags: u32) -> bool {
    skip_prefix(pattern, subject.len(), flags) == subject
}

/// Only the domain of an address compares without case (RFC 5280, section 7.5).
fn equal_email(a: &[u8], b: &[u8], _flags: u32) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut at = a.len();
    while at > 0 {
        at -= 1;
        if a[at] == b'@' || b[at] == b'@' {
            if !equal_nocase(&a[at..], &b[at..], 0) {
                return false;
            }
            break;
        }
    }
    if at == 0 {
        at = a.len();
    }
    a[..at] == b[..at]
}

fn has_idna_prefix(text: &[u8]) -> bool {
    text.len() >= 4 && text[..4].eq_ignore_ascii_case(b"xn--")
}

fn wildcard_match(prefix: &[u8], suffix: &[u8], subject: &[u8], flags: u32) -> bool {
    if subject.len() < prefix.len() + suffix.len() {
        return false;
    }
    let (start, end) = (prefix.len(), subject.len() - suffix.len());
    if !equal_nocase(prefix, &subject[..start], flags) || !equal_nocase(&subject[end..], suffix, flags) {
        return false;
    }
    let mut allow_multi = false;
    let mut allow_idna = false;
    if prefix.is_empty() && suffix.first() == Some(&b'.') {
        if start == end {
            return false;
        }
        allow_idna = true;
        allow_multi = flags & MULTI_LABEL_WILDCARDS != 0;
    }
    if !allow_idna && has_idna_prefix(subject) {
        return false;
    }
    if end == start + 1 && subject[start] == b'*' {
        return true;
    }
    subject[start..end].iter().all(|&c| c.is_ascii_alphanumeric() || c == b'-' || (allow_multi && c == b'.'))
}

/// The position of the one legal wildcard: at the start or end of a non-IDNA first label.
fn valid_star(pattern: &[u8], flags: u32) -> Option<usize> {
    const START: u8 = 1;
    const HYPHEN: u8 = 4;
    const IDNA: u8 = 8;
    let (mut star, mut state, mut dots) = (None, START, 0);
    for (index, &c) in pattern.iter().enumerate() {
        if c == b'*' {
            let at_start = state & START != 0;
            let at_end = index == pattern.len() - 1 || pattern[index + 1] == b'.';
            if star.is_some() || state & IDNA != 0 || dots > 0 {
                return None;
            }
            if (flags & NO_PARTIAL_WILDCARDS != 0 && !(at_start && at_end)) || (!at_start && !at_end) {
                return None;
            }
            star = Some(index);
            state &= !START;
        } else if c.is_ascii_alphanumeric() {
            if state & START != 0 && has_idna_prefix(&pattern[index..]) {
                state |= IDNA;
            }
            state &= !(HYPHEN | START);
        } else if c == b'.' {
            if state & (HYPHEN | START) != 0 {
                return None;
            }
            state = START;
            dots += 1;
        } else if c == b'-' {
            if state & START != 0 {
                return None;
            }
            state |= HYPHEN;
        } else {
            return None;
        }
    }
    if state & (START | HYPHEN) != 0 || dots < 2 {
        return None;
    }
    star
}

fn equal_wildcard(pattern: &[u8], subject: &[u8], flags: u32) -> bool {
    let star = if subject.len() > 1 && subject[0] == b'.' { None } else { valid_star(pattern, flags) };
    match star {
        Some(index) => wildcard_match(&pattern[..index], &pattern[index + 1..], subject, flags),
        None => equal_nocase(pattern, subject, flags),
    }
}

fn until_nul(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().take_while(|byte| **byte != 0).copied().collect()
}

/// `do_check_string`: the matched value on a match. A positive `cmp_type` requires that string
/// type; otherwise the value converts to UTF-8 first.
fn check_string(
    tag: u8,
    value: &[u8],
    cmp_type: u8,
    equal: Equal,
    flags: u32,
    chk: &[u8],
) -> Result<Option<Vec<u8>>, JsError> {
    if value.is_empty() {
        return Ok(None);
    }
    if cmp_type > 0 {
        let matched = tag == cmp_type && if cmp_type == IA5 { equal(value, chk, flags) } else { value == chk };
        return Ok(matched.then(|| until_nul(value)));
    }
    let text =
        to_utf8(tag, value).ok_or_else(|| JsError::new("Operation failed").with_code("ERR_CRYPTO_OPERATION_FAILED"))?;
    Ok(equal(text.as_bytes(), chk, flags).then(|| until_nul(text.as_bytes())))
}

fn ipv4(text: &[u8]) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut rest = text;
    for (index, slot) in out.iter_mut().enumerate() {
        if index > 0 {
            rest = rest.strip_prefix(b".")?;
        }
        let mut value = 0u32;
        loop {
            let digit = rest.first().filter(|c| c.is_ascii_digit())?;
            value = value * 10 + u32::from(digit - b'0');
            if value > 255 {
                return None;
            }
            rest = &rest[1..];
            if rest.first().is_none_or(|c| *c == b'.') {
                break;
            }
            if value == 0 {
                return None;
            }
        }
        *slot = value as u8;
    }
    rest.is_empty().then_some(out)
}

fn ipv6(text: &[u8]) -> Option<[u8; 16]> {
    let mut parts = [0u8; 16];
    let (mut total, mut zero_pos, mut zero_count) = (0usize, None, 0);
    let elements: Vec<&[u8]> = text.split(|c| *c == b':').collect();
    for (index, element) in elements.iter().enumerate() {
        if total == 16 {
            return None;
        }
        if element.is_empty() {
            match zero_pos {
                None => zero_pos = Some(total),
                Some(position) if position != total => return None,
                _ => {}
            }
            zero_count += 1;
        } else if element.len() > 4 {
            if total > 12 || index != elements.len() - 1 {
                return None;
            }
            parts[total..total + 4].copy_from_slice(&ipv4(element)?);
            total += 4;
        } else {
            let value =
                element.iter().try_fold(0u16, |value, c| Some((value << 4) | (*c as char).to_digit(16)? as u16))?;
            parts[total..total + 2].copy_from_slice(&value.to_be_bytes());
            total += 2;
        }
    }
    let Some(position) = zero_pos else { return (total == 16).then_some(parts) };
    let edge = position == 0 || position == total;
    if total == 16
        || zero_count > 3
        || (zero_count == 3 && total > 0)
        || (zero_count == 2 && !edge)
        || (zero_count == 1 && edge)
    {
        return None;
    }
    let mut out = [0u8; 16];
    out[..position].copy_from_slice(&parts[..position]);
    out[position + 16 - total..].copy_from_slice(&parts[position..total]);
    Some(out)
}

fn invalid(message: &str) -> JsError {
    JsError::type_error(message).with_code("ERR_INVALID_ARG_VALUE")
}

/// Match `input` against the subjectAltName entries (`names`, when the extension decodes) and,
/// as OpenSSL allows, the subject. Returns the matched value, or `None`.
pub(super) fn check(
    names: Option<&[(u8, &[u8])]>,
    subject: &[Attribute],
    kind: Kind,
    input: &str,
    flags: u32,
) -> Result<Option<Vec<u8>>, JsError> {
    let mut flags = flags & !DOT_SUBDOMAINS;
    let mut chk = input.as_bytes().to_vec();
    if kind == Kind::Ip {
        let parsed = if chk.contains(&b':') { ipv6(&chk).map(Vec::from) } else { ipv4(&chk).map(Vec::from) };
        chk = parsed.ok_or_else(|| invalid("Invalid IP"))?;
    } else if chk.len() > 1 {
        if chk[..chk.len() - 1].contains(&0) {
            return Err(invalid("Invalid name"));
        }
        if chk.last() == Some(&0) {
            chk.pop();
        }
    } else if chk == [0] {
        return Err(invalid("Invalid name"));
    }
    let (common_name, alt_tag, mut alt_type, equal): (Option<&str>, u8, u8, Equal) = match kind {
        Kind::Email => (Some("1.2.840.113549.1.9.1"), 0x81, IA5, equal_email),
        Kind::Host => {
            if chk.len() > 1 && chk[0] == b'.' {
                flags |= DOT_SUBDOMAINS;
            }
            let equal: Equal = if flags & NO_WILDCARDS != 0 { equal_nocase } else { equal_wildcard };
            (Some("2.5.4.3"), 0x82, IA5, equal)
        }
        Kind::Ip => (None, 0x87, OCTETS, equal_case),
    };
    if let Some(names) = names {
        let mut present = false;
        for &(tag, content) in names {
            let (value_tag, value) = if tag == alt_tag {
                (if kind == Kind::Ip { OCTETS } else { IA5 }, content)
            } else if tag == 0xa0 && kind == Kind::Email {
                // RFC 8398 SmtpUTF8Mailbox, a UTF8String; OpenSSL then converts the later names too.
                let mut other = Der::new(content);
                let Ok("1.3.6.1.5.5.7.8.9") = other.oid().as_deref() else { continue };
                let Some((12, value)) = other.expect(0xa0).ok().and_then(|inner| Der::new(inner).read().ok()) else {
                    continue;
                };
                alt_type = 0;
                (12, value)
            } else {
                continue;
            };
            present = true;
            if let Some(found) = check_string(value_tag, value, alt_type, equal, flags, &chk)? {
                return Ok(Some(found));
            }
        }
        if present && flags & ALWAYS_CHECK_SUBJECT == 0 {
            return Ok(None);
        }
    }
    let Some(common_name) = common_name.filter(|_| flags & NEVER_CHECK_SUBJECT == 0) else { return Ok(None) };
    for attribute in subject.iter().filter(|attribute| attribute.oid == common_name) {
        if let Some(found) = check_string(attribute.tag, attribute.value, 0, equal, flags, &chk)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}
