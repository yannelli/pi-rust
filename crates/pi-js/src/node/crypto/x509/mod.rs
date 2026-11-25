//! Natives for `crypto.X509Certificate`: parsing, the fields Node reports, host checks, issuer
//! checks, and signature verification. Certificates travel as latin1 strings of their DER.

mod check;
mod name;
mod verify;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rquickjs::{Ctx, Object};
use serde_json::{Value, json};

use super::der::Der;
use super::keys::Key;
use crate::error::JsError;
use crate::node::set_fn;
use crate::runtime::convert::{latin1_to_bytes as bytes, throw};

struct Extension<'a> {
    oid: String,
    value: &'a [u8],
}

/// A parsed certificate. Names are the contents of their SEQUENCEs; algorithms and the SPKI
/// are whole elements; `signature` is the BIT STRING content.
struct Certificate<'a> {
    der: &'a [u8],
    tbs: &'a [u8],
    serial: &'a [u8],
    tbs_algorithm: &'a [u8],
    issuer: &'a [u8],
    validity: [(u8, &'a [u8]); 2],
    subject: &'a [u8],
    spki: &'a [u8],
    extensions: Vec<Extension<'a>>,
    algorithm: &'a [u8],
    signature: &'a [u8],
}

fn element<'a>(reader: &mut Der<'a>, tag: u8) -> Result<&'a [u8], JsError> {
    match reader.raw()? {
        raw if raw[0] == tag => Ok(raw),
        _ => Err(wrong_tag()),
    }
}

impl<'a> Certificate<'a> {
    fn parse(der: &'a [u8]) -> Result<Self, JsError> {
        let mut outer = Der::new(der).seq()?;
        let tbs = element(&mut outer, 0x30)?;
        let algorithm = element(&mut outer, 0x30)?;
        let signature = outer.expect(0x03)?;
        let mut fields = Der::new(tbs).seq()?;
        if fields.peek() == Some(0xa0) {
            Der::new(fields.expect(0xa0)?).uint()?;
        }
        let serial = fields.expect(0x02)?;
        let tbs_algorithm = element(&mut fields, 0x30)?;
        let issuer = fields.expect(0x30)?;
        let mut validity = fields.seq()?;
        let validity = [validity.read()?, validity.read()?];
        let subject = fields.expect(0x30)?;
        let spki = element(&mut fields, 0x30)?;
        for tag in [0x81, 0xa1, 0x82, 0xa2] {
            if fields.peek() == Some(tag) {
                fields.read()?;
            }
        }
        let mut extensions = Vec::new();
        if fields.peek() == Some(0xa3) {
            let mut list = Der::new(Der::new(fields.expect(0xa3)?).expect(0x30)?);
            while !list.is_empty() {
                let mut extension = list.seq()?;
                let oid = extension.oid()?;
                if extension.peek() == Some(0x01) {
                    extension.expect(0x01)?;
                }
                extensions.push(Extension { oid, value: extension.expect(0x04)? });
            }
        }
        let times_ok = validity.iter().all(|(tag, _)| matches!(tag, 0x17 | 0x18));
        if !outer.is_empty() || !fields.is_empty() || !times_ok {
            return Err(wrong_tag());
        }
        name::attributes(issuer)?;
        name::attributes(subject)?;
        Ok(Self { der, tbs, serial, tbs_algorithm, issuer, validity, subject, spki, extensions, algorithm, signature })
    }

    fn extension(&self, oid: &str) -> Option<&'a [u8]> {
        self.extensions.iter().find(|extension| extension.oid == oid).map(|extension| extension.value)
    }
}

fn ossl(code: &str, message: &str) -> JsError {
    JsError::new(message).with_code(code)
}

fn wrong_tag() -> JsError {
    ossl("ERR_OSSL_ASN1_WRONG_TAG", "error:068000A8:asn1 encoding routines::wrong tag")
}

fn too_long() -> JsError {
    ossl("ERR_OSSL_ASN1_TOO_LONG", "error:0680009B:asn1 encoding routines::too long")
}

/// The first element of `data` whole, with the errors OpenSSL's `d2i` gives for its header.
fn outer_element(data: &[u8]) -> Result<&[u8], JsError> {
    let header_too_long =
        || ossl("ERR_OSSL_ASN1_HEADER_TOO_LONG", "error:0680007B:asn1 encoding routines::header too long");
    let (&tag, rest) = data.split_first().ok_or_else(wrong_tag)?;
    let (&first, rest) = rest.split_first().ok_or_else(header_too_long)?;
    if tag != 0x30 {
        return Err(wrong_tag());
    }
    let (header, length) = match first {
        0..=0x7f => (2, usize::from(first)),
        0x81..=0x84 => {
            let count = usize::from(first & 0x7f);
            let digits = rest.get(..count).ok_or_else(header_too_long)?;
            (2 + count, digits.iter().fold(0usize, |total, byte| (total << 8) | usize::from(*byte)))
        }
        _ => return Err(wrong_tag()),
    };
    data.get(..header + length).ok_or_else(too_long)
}

/// `d2i_X509_AUX` on a PEM body: one certificate, optionally followed by trust settings.
fn pem_certificate(data: &[u8]) -> Result<Vec<u8>, JsError> {
    let der = outer_element(data)?;
    let rest = &data[der.len()..];
    if !rest.is_empty() && Der::new(rest).seq().is_err() {
        return Err(too_long());
    }
    Certificate::parse(der).map_err(|_| wrong_tag())?;
    Ok(der.to_vec())
}

/// The error for a PEM block with headers. Only encryption headers are valid, and nothing
/// supplies a passphrase, so decryption fails.
fn pem_headers(body: &[&[u8]]) -> JsError {
    match body {
        [first, ..] if !first.starts_with(b"Proc-Type: ") => {
            ossl("ERR_OSSL_PEM_NOT_PROC_TYPE", "error:0480006B:PEM routines::not proc type")
        }
        [first, ..] if !first.ends_with(b",ENCRYPTED") => {
            ossl("ERR_OSSL_PEM_NOT_ENCRYPTED", "error:0480006A:PEM routines::not encrypted")
        }
        [_, second, ..] if second.starts_with(b"DEK-Info: ") => {
            ossl("ERR_OSSL_WRONG_FINAL_BLOCK_LENGTH", "error:1C80006B:Provider routines::wrong final block length")
        }
        _ => ossl("ERR_OSSL_PEM_NOT_DEK_INFO", "error:04800069:PEM routines::not dek info"),
    }
}

/// The DER of the certificate in `input`: the first PEM certificate block, else DER bytes.
fn certificate_der(input: &[u8]) -> Result<Vec<u8>, JsError> {
    let mut lines = input.split(|byte| *byte == b'\n').map(<[u8]>::trim_ascii_end);
    while let Some(line) = lines.next() {
        let Some(label) = line.strip_prefix(b"-----BEGIN ").and_then(|rest| rest.strip_suffix(b"-----")) else {
            continue;
        };
        if ![&b"CERTIFICATE"[..], b"X509 CERTIFICATE", b"TRUSTED CERTIFICATE"].contains(&label) {
            continue;
        }
        let end = [b"-----END ", label, b"-----"].concat();
        let mut body = Vec::new();
        loop {
            let line = lines.next().unwrap_or(b"-----END ");
            if line.starts_with(b"-----END ") {
                if line != end.as_slice() {
                    return Err(ossl("ERR_OSSL_PEM_BAD_END_LINE", "error:04800066:PEM routines::bad end line"));
                }
                break;
            }
            body.push(line);
        }
        if body.first().is_some_and(|line| line.contains(&b':')) {
            return Err(pem_headers(&body));
        }
        let der = STANDARD
            .decode(body.concat())
            .map_err(|_| ossl("ERR_OSSL_PEM_BAD_BASE64_DECODE", "error:04800064:PEM routines::bad base64 decode"))?;
        return pem_certificate(&der);
    }
    outer_element(input)
        .ok()
        .filter(|der| Certificate::parse(der).is_ok())
        .map(<[u8]>::to_vec)
        .ok_or_else(|| ossl("ERR_OSSL_PEM_NO_START_LINE", "error:0480006C:PEM routines::no start line"))
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let month = if shifted < 10 { shifted + 3 } else { shifted - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day_of_year - (153 * shifted + 2) / 5 + 1)
}

/// A UTCTime or GeneralizedTime as seconds since the epoch and as `ASN1_TIME_print` shows it.
fn time(tag: u8, value: &[u8]) -> Option<(i64, String)> {
    let text = std::str::from_utf8(value).ok()?;
    let number = |at: usize, length: usize| {
        let part = text.get(at..at + length)?;
        part.bytes().all(|byte| byte.is_ascii_digit()).then(|| part.parse::<i64>().ok())?
    };
    let year_length = if tag == 0x17 { 2 } else { 4 };
    let mut year = number(0, year_length)?;
    if tag == 0x17 {
        year += if year < 50 { 2000 } else { 1900 };
    }
    let [month, day, hour, minute] = [0, 1, 2, 3].map(|index| number(year_length + 2 * index, 2));
    let (month, day, hour, minute) = (month?, day?, hour?, minute?);
    let mut at = year_length + 8;
    let second = number(at, 2).inspect(|_| at += 2).unwrap_or(0);
    let mut fraction = "";
    if tag == 0x18 && at == year_length + 10 && text[at..].starts_with('.') {
        let digits = text[at + 1..].bytes().take_while(u8::is_ascii_digit).count();
        fraction = text.get(at..at + 1 + digits).filter(|_| digits > 0)?;
        at += 1 + digits;
    }
    let offset = match text.get(at..)? {
        "Z" => 0,
        zone if zone.len() == 5 && (zone.starts_with('+') || zone.starts_with('-')) => {
            let (hours, minutes) =
                (number(at + 1, 2).filter(|hours| *hours <= 12)?, number(at + 3, 2).filter(|minutes| *minutes <= 59)?);
            (hours * 3600 + minutes * 60) * if zone.starts_with('-') { -1 } else { 1 }
        }
        _ => return None,
    };
    let month_days =
        days_from_civil(year + i64::from(month == 12), month % 12 + 1, 1) - days_from_civil(year, month, 1);
    if !(1..=12).contains(&month) || !(1..=month_days).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let clock = seconds.rem_euclid(86_400);
    let name = MONTHS[(month - 1) as usize];
    let (hour, minute, second) = (clock / 3600, clock / 60 % 60, clock % 60);
    Some((seconds, format!("{name} {day:>2} {hour:02}:{minute:02}:{second:02}{fraction} {year} GMT")))
}

fn fingerprint(algorithm: &'static ring::digest::Algorithm, der: &[u8]) -> String {
    let digest = ring::digest::digest(algorithm, der);
    digest.as_ref().iter().map(|byte| format!("{byte:02X}")).collect::<Vec<_>>().join(":")
}

/// `BN_bn2hex` of an INTEGER's two's complement content.
fn serial_hex(content: &[u8]) -> String {
    let negative = content.first().is_some_and(|byte| byte & 0x80 != 0);
    let mut magnitude = content.to_vec();
    if negative {
        let mut carry = true;
        for byte in magnitude.iter_mut().rev() {
            *byte = !*byte;
            if carry {
                (*byte, carry) = byte.overflowing_add(1);
            }
        }
    }
    let start = magnitude.iter().position(|byte| *byte != 0).unwrap_or(magnitude.len());
    let hex: String = magnitude[start..].iter().map(|byte| format!("{byte:02X}")).collect();
    match (negative, hex.is_empty()) {
        (_, true) => "0".to_string(),
        (true, false) => format!("-{hex}"),
        (false, false) => hex,
    }
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| char::from(*byte)).collect()
}

/// `toLegacyObject` key fields: RSA modulus and exponent, or the EC point and curve names.
fn legacy_key(spki: &[u8]) -> Value {
    let Ok(key) = Key::parse("der", "spki", spki, None) else { return json!([]) };
    match &key {
        Key::Rsa { pss: false, rsa } => {
            let modulus: String = rsa.n().iter().map(|byte| format!("{byte:02X}")).collect();
            let exponent = rsa.e().iter().fold(0u128, |total, byte| (total << 8) | u128::from(*byte));
            json!([
                ["modulus", modulus.trim_start_matches('0')],
                ["bits", rsa.bits()],
                ["exponent", format!("0x{:x}", u64::try_from(exponent).unwrap_or(u64::MAX))],
                ["pubkey", latin1(&key.spki())],
            ])
        }
        Key::Ec { curve, .. } => {
            let mut info = Der::new(spki).seq().ok();
            let point = info.as_mut().and_then(|info| info.raw().ok().and(info.bits().ok()));
            let bits = if curve.size == 66 { 521 } else { curve.size * 8 };
            let mut fields = vec![json!(["bits", bits]), json!(["pubkey", latin1(point.unwrap_or_default())])];
            fields.push(json!(["asn1Curve", curve.name]));
            if curve.jwk.starts_with("P-") {
                fields.push(json!(["nistCurve", curve.jwk]));
            }
            Value::Array(fields)
        }
        _ => json!([]),
    }
}

fn name_pairs(attributes: &[name::Attribute]) -> Value {
    attributes
        .iter()
        .map(|attribute| {
            let key = name::short_name(&attribute.oid).map_or_else(|| attribute.oid.clone(), str::to_string);
            json!([key, name::to_utf8(attribute.tag, attribute.value)])
        })
        .collect()
}

/// Every field Node derives from the certificate, for the JavaScript class to cache.
fn info(cert: &Certificate) -> Value {
    let subject = name::attributes(cert.subject).unwrap_or_default();
    let issuer = name::attributes(cert.issuer).unwrap_or_default();
    let print = |list: &[name::Attribute]| {
        name::print_name(list, false)
            .filter(|text| !text.is_empty())
            .map(|text| String::from_utf8_lossy(&text).into_owned())
    };
    let times = cert.validity.map(|(tag, value)| time(tag, value));
    let text = |index: usize| times[index].as_ref().map_or("Bad time value", |(_, text)| text.as_str()).to_string();
    let millis = |index: usize| times[index].as_ref().map(|(seconds, _)| seconds * 1000);
    let key_usage = verify::decode(&mut false, cert, verify::EXT_KEY_USAGE, verify::object_ids);
    json!({
        "subject": print(&subject),
        "issuer": print(&issuer),
        "subjectAltName": cert.extension(verify::SUBJECT_ALT_NAME).and_then(name::print_alt_names),
        "infoAccess": cert.extension("1.3.6.1.5.5.7.1.1").and_then(name::print_info_access),
        "validFrom": text(0),
        "validTo": text(1),
        "validFromTime": millis(0),
        "validToTime": millis(1),
        "fingerprint": fingerprint(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, cert.der),
        "fingerprint256": fingerprint(&ring::digest::SHA256, cert.der),
        "fingerprint512": fingerprint(&ring::digest::SHA512, cert.der),
        "keyUsage": key_usage,
        "serialNumber": serial_hex(cert.serial),
        "ca": verify::flags(cert).is_ca(),
        "spki": latin1(cert.spki),
        "legacySubject": name_pairs(&subject),
        "legacyIssuer": name_pairs(&issuer),
        "legacyKey": legacy_key(cert.spki),
    })
}

fn with_cert<T>(der: &str, f: impl FnOnce(&Certificate) -> T) -> Result<T, JsError> {
    let der = bytes(der);
    Ok(f(&Certificate::parse(&der)?))
}

pub(super) fn register<'js>(object: &Object<'js>) -> rquickjs::Result<()> {
    set_fn(object, "x509Parse", |ctx: Ctx<'js>, data: String| {
        certificate_der(&bytes(&data)).map(|der| latin1(&der)).map_err(|error| throw(&ctx, &error))
    })?;
    set_fn(object, "x509Info", |ctx: Ctx<'js>, der: String| {
        with_cert(&der, |cert| info(cert).to_string()).map_err(|error| throw(&ctx, &error))
    })?;
    set_fn(object, "x509Check", |ctx: Ctx<'js>, der: String, kind: String, input: String, flags: u32| {
        let kind = match kind.as_str() {
            "host" => check::Kind::Host,
            "email" => check::Kind::Email,
            _ => check::Kind::Ip,
        };
        let found = with_cert(&der, |cert| {
            let names =
                verify::decode(&mut false, cert, verify::SUBJECT_ALT_NAME, |value| name::general_names(value).ok());
            let subject = name::attributes(cert.subject).unwrap_or_default();
            check::check(names.as_deref(), &subject, kind, &input, flags)
        })
        .and_then(|found| found)
        .map_err(|error| throw(&ctx, &error))?;
        Ok::<_, rquickjs::Error>(
            found.map(|peer| if kind == check::Kind::Host { latin1(&peer) } else { input.clone() }),
        )
    })?;
    set_fn(object, "x509Issued", |ctx: Ctx<'js>, der: String, issuer: String| {
        with_cert(&der, |cert| with_cert(&issuer, |issuer| verify::issued(cert, issuer)))
            .and_then(|issued| issued)
            .map_err(|error| throw(&ctx, &error))
    })?;
    set_fn(object, "x509Verify", |ctx: Ctx<'js>, der: String, spki: String| {
        with_cert(&der, |cert| verify::signature_valid(cert, &bytes(&spki))).map_err(|error| throw(&ctx, &error))
    })?;
    set_fn(object, "x509CheckPrivateKey", |ctx: Ctx<'js>, der: String, pkcs8: String| {
        with_cert(&der, |cert| verify::private_key_matches(cert, &bytes(&pkcs8))).map_err(|error| throw(&ctx, &error))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_times_and_serials_as_openssl() {
        let text = |tag, value: &str| time(tag, value.as_bytes()).map(|(_, text)| text);
        assert_eq!(text(0x18, "20250102030405.123Z").as_deref(), Some("Jan  2 03:04:05.123 2025 GMT"));
        assert_eq!(text(0x17, "2501020304+0130").as_deref(), Some("Jan  2 01:34:00 2025 GMT"));
        assert_eq!(time(0x17, b"491231235959Z").map(|(seconds, _)| seconds), Some(2_524_607_999));
        assert_eq!(text(0x17, "250230000000Z"), None);
        assert_eq!(text(0x18, "20250102030405.Z"), None);
        assert_eq!(
            (serial_hex(&[0]), serial_hex(&[0xff]), serial_hex(&[0x00, 0x80])),
            ("0".into(), "-01".into(), "80".into())
        );
    }
}
