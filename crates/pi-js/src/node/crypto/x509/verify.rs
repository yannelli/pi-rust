//! OpenSSL's extension cache (`ossl_x509v3_cache_extensions`) as far as `X509_check_ca` and
//! `X509_check_issued` need it, plus `X509_verify` and `X509_check_private_key`.

use super::super::der::Der;
use super::super::keys::Key;
use super::super::ops::{self, SignOptions};
use super::super::rsa;
use super::Certificate;
use super::name::{attributes, general_names, to_utf8};

const BASIC_CONSTRAINTS: &str = "2.5.29.19";
const KEY_USAGE: &str = "2.5.29.15";
pub(super) const EXT_KEY_USAGE: &str = "2.5.29.37";
const SUBJECT_KEY_ID: &str = "2.5.29.14";
const AUTHORITY_KEY_ID: &str = "2.5.29.35";
pub(super) const SUBJECT_ALT_NAME: &str = "2.5.29.17";
const ISSUER_ALT_NAME: &str = "2.5.29.18";
const NAME_CONSTRAINTS: &str = "2.5.29.30";
const CRL_DISTRIBUTION_POINTS: &str = "2.5.29.31";
const NETSCAPE_CERT_TYPE: &str = "2.16.840.1.113730.1.1";
const PROXY_CERT_INFO: &str = "1.3.6.1.5.5.7.1.14";
const KU_DIGITAL_SIGNATURE: u32 = 0x80;
const KU_KEY_CERT_SIGN: u32 = 0x04;

#[derive(Default)]
struct Akid<'a> {
    keyid: Option<&'a [u8]>,
    issuer: Option<&'a [u8]>,
    serial: Option<&'a [u8]>,
}

pub(super) struct Flags<'a> {
    invalid: bool,
    basic_ca: Option<bool>,
    key_usage: Option<u32>,
    proxy: bool,
    skid: Option<&'a [u8]>,
    akid: Option<Akid<'a>>,
}

/// `X509_get_ext_d2i`: the single extension with `oid` decoded by `parse`. A duplicate or a
/// value that does not decode marks the certificate invalid.
pub(super) fn decode<'a, T>(
    invalid: &mut bool,
    cert: &Certificate<'a>,
    oid: &str,
    parse: impl FnOnce(&'a [u8]) -> Option<T>,
) -> Option<T> {
    let mut found = cert.extensions.iter().filter(|extension| extension.oid == oid);
    let first = found.next()?;
    let value = if found.next().is_some() { None } else { parse(first.value) };
    *invalid |= value.is_none();
    value
}

fn sequence(value: &[u8]) -> Option<()> {
    Der::new(value).seq().ok().map(|_| ())
}

/// `(cA, negative pathLenConstraint)` of basicConstraints.
fn basic_constraints(value: &[u8]) -> Option<(bool, bool)> {
    let mut fields = Der::new(value).seq().ok()?;
    let ca = fields.peek() == Some(0x01) && fields.expect(0x01).ok()?.iter().any(|byte| *byte != 0);
    let negative =
        fields.peek() == Some(0x02) && fields.expect(0x02).ok()?.first().is_some_and(|byte| byte & 0x80 != 0);
    fields.is_empty().then_some((ca, negative))
}

fn key_usage(value: &[u8]) -> Option<u32> {
    let bits = Der::new(value).expect(0x03).ok()?;
    let byte = |index: usize| u32::from(bits.get(index).copied().unwrap_or(0));
    (!bits.is_empty()).then(|| byte(1) | (byte(2) << 8))
}

pub(super) fn object_ids(value: &[u8]) -> Option<Vec<String>> {
    let mut list = Der::new(value).seq().ok()?;
    let mut oids = Vec::new();
    while !list.is_empty() {
        oids.push(list.oid().ok()?);
    }
    Some(oids)
}

fn authority_key_id(value: &[u8]) -> Option<Akid<'_>> {
    let mut fields = Der::new(value).seq().ok()?;
    let mut akid = Akid::default();
    if fields.peek() == Some(0x80) {
        akid.keyid = Some(fields.expect(0x80).ok()?);
    }
    if fields.peek() == Some(0xa1) {
        let mut names = Der::new(fields.expect(0xa1).ok()?);
        while !names.is_empty() {
            let (tag, content) = names.read().ok()?;
            if tag == 0xa4 && akid.issuer.is_none() {
                akid.issuer = Some(Der::new(content).expect(0x30).ok()?);
            }
        }
    }
    if fields.peek() == Some(0x82) {
        akid.serial = Some(fields.expect(0x82).ok()?);
    }
    fields.is_empty().then_some(akid)
}

pub(super) fn flags<'a>(cert: &Certificate<'a>) -> Flags<'a> {
    let mut invalid = false;
    let basic = decode(&mut invalid, cert, BASIC_CONSTRAINTS, basic_constraints);
    invalid |= basic.is_some_and(|(_, negative)| negative);
    let present = |oid: &str| cert.extensions.iter().any(|extension| extension.oid == oid);
    let proxy = decode(&mut invalid, cert, PROXY_CERT_INFO, sequence).is_some();
    invalid |= proxy && (basic.is_some_and(|(ca, _)| ca) || present(SUBJECT_ALT_NAME) || present(ISSUER_ALT_NAME));
    let key_usage = decode(&mut invalid, cert, KEY_USAGE, key_usage);
    invalid |= key_usage == Some(0);
    decode(&mut invalid, cert, EXT_KEY_USAGE, object_ids);
    decode(&mut invalid, cert, NETSCAPE_CERT_TYPE, |value| Der::new(value).expect(0x03).ok().map(|_| ()));
    let skid = decode(&mut invalid, cert, SUBJECT_KEY_ID, |value| Der::new(value).expect(0x04).ok());
    let akid = decode(&mut invalid, cert, AUTHORITY_KEY_ID, authority_key_id);
    decode(&mut invalid, cert, SUBJECT_ALT_NAME, |value| general_names(value).ok());
    decode(&mut invalid, cert, NAME_CONSTRAINTS, sequence);
    decode(&mut invalid, cert, CRL_DISTRIBUTION_POINTS, sequence);
    Flags { invalid, basic_ca: basic.map(|(ca, _)| ca), key_usage, proxy, skid, akid }
}

impl Flags<'_> {
    fn rejects(&self, usage: u32) -> bool {
        self.key_usage.is_some_and(|bits| bits & usage == 0)
    }

    /// `X509_check_ca(cert) == 1`, which Node reports as `ca`.
    pub(super) fn is_ca(&self) -> bool {
        !self.invalid && !self.rejects(KU_KEY_CERT_SIGN) && self.basic_ca == Some(true)
    }
}

/// An attribute value as `X509_NAME_cmp` compares it: string types as trimmed, lowercased UTF-8
/// with single spaces, other types as their bytes.
fn canonical_value(tag: u8, value: &[u8]) -> Option<(u8, Vec<u8>)> {
    if !matches!(tag, 12 | 19 | 20 | 22 | 26 | 28 | 30) {
        return Some((tag, value.to_vec()));
    }
    let text = to_utf8(tag, value)?;
    let space = |byte: &u8| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r');
    let mut out = Vec::new();
    for word in text.as_bytes().split(space).filter(|word| !word.is_empty()) {
        if !out.is_empty() {
            out.push(b' ');
        }
        out.extend(word.iter().map(u8::to_ascii_lowercase));
    }
    Some((0, out))
}

/// `(RDN index, attribute type, canonical value)`.
type CanonicalAttribute = (usize, String, (u8, Vec<u8>));

fn canonical_name(name: &[u8]) -> Option<Vec<CanonicalAttribute>> {
    let list = attributes(name).ok()?;
    list.into_iter()
        .map(|attribute| Some((attribute.set, attribute.oid, canonical_value(attribute.tag, attribute.value)?)))
        .collect()
}

pub(super) fn same_name(a: &[u8], b: &[u8]) -> bool {
    matches!((canonical_name(a), canonical_name(b)), (Some(a), Some(b)) if a == b)
}

fn algorithm_oid(algorithm: &[u8]) -> Option<String> {
    Der::new(algorithm).seq().ok()?.oid().ok()
}

/// The key type a signature algorithm needs, and its digest.
fn signature_scheme(oid: &str) -> Option<(&'static str, &'static str)> {
    Some(match oid {
        "1.2.840.113549.1.1.4" => ("rsa", "md5"),
        "1.2.840.113549.1.1.5" => ("rsa", "sha1"),
        "1.2.840.113549.1.1.11" => ("rsa", "sha256"),
        "1.2.840.113549.1.1.12" => ("rsa", "sha384"),
        "1.2.840.113549.1.1.13" => ("rsa", "sha512"),
        "1.2.840.113549.1.1.10" => ("rsa-pss", ""),
        "1.2.840.10045.4.1" => ("ec", "sha1"),
        "1.2.840.10045.4.3.2" => ("ec", "sha256"),
        "1.2.840.10045.4.3.3" => ("ec", "sha384"),
        "1.2.840.10045.4.3.4" => ("ec", "sha512"),
        "1.2.840.10040.4.3" => ("dsa", "sha1"),
        "2.16.840.1.101.3.4.3.2" => ("dsa", "sha256"),
        "1.3.101.112" => ("ed25519", ""),
        "1.3.101.113" => ("ed448", ""),
        _ => return None,
    })
}

fn key_fits(key: &Key, needed: &str) -> bool {
    let kind = key.type_name();
    kind == needed || (kind == "rsa" && needed == "rsa-pss")
}

fn public_key(spki: &[u8]) -> Option<Key> {
    Key::parse("der", "spki", spki, None).ok()
}

/// `X509_check_issued(issuer, subject) == X509_V_OK`: names, key identifiers, the signature
/// algorithm family, and key usage. It does not check the signature.
pub(super) fn issued(subject: &Certificate, issuer: &Certificate) -> bool {
    if !same_name(issuer.subject, subject.issuer) {
        return false;
    }
    let (subject_flags, issuer_flags) = (flags(subject), flags(issuer));
    if subject_flags.invalid || issuer_flags.invalid {
        return false;
    }
    if let Some(akid) = &subject_flags.akid {
        let keyid_differs = matches!((akid.keyid, issuer_flags.skid), (Some(keyid), Some(skid)) if keyid != skid);
        let serial_differs = akid.serial.is_some_and(|serial| serial != issuer.serial);
        let name_differs = akid.issuer.is_some_and(|name| !same_name(name, issuer.issuer));
        if keyid_differs || serial_differs || name_differs {
            return false;
        }
    }
    let needed = algorithm_oid(subject.tbs_algorithm).as_deref().and_then(signature_scheme);
    let fits = matches!((public_key(issuer.spki), needed), (Some(key), Some((kind, _))) if key_fits(&key, kind));
    let usage = if subject_flags.proxy { KU_DIGITAL_SIGNATURE } else { KU_KEY_CERT_SIGN };
    fits && !issuer_flags.rejects(usage)
}

fn digest_name(oid: &str) -> Option<&'static str> {
    Some(match oid {
        "1.3.14.3.2.26" => "sha1",
        "2.16.840.1.101.3.4.2.1" => "sha256",
        "2.16.840.1.101.3.4.2.2" => "sha384",
        "2.16.840.1.101.3.4.2.3" => "sha512",
        _ => return None,
    })
}

/// RSASSA-PSS parameters: digest and salt length. The MGF1 digest must match the digest.
fn pss_params(algorithm: &[u8]) -> Option<(&'static str, i32)> {
    let mut outer = Der::new(algorithm).seq().ok()?;
    outer.oid().ok()?;
    let mut params = outer.seq().ok()?;
    let mut hash = "sha1";
    let mut mgf_hash = "sha1";
    let mut salt = 20;
    if params.peek() == Some(0xa0) {
        hash = digest_name(&algorithm_oid(params.expect(0xa0).ok()?)?)?;
    }
    if params.peek() == Some(0xa1) {
        let mut mgf = Der::new(params.expect(0xa1).ok()?).seq().ok()?;
        (mgf.oid().ok()? == "1.2.840.113549.1.1.8").then_some(())?;
        mgf_hash = digest_name(&algorithm_oid(mgf.raw().ok()?)?)?;
    }
    if params.peek() == Some(0xa2) {
        let value = Der::new(params.expect(0xa2).ok()?).uint().ok()?;
        salt = i32::try_from(value.iter().fold(0u64, |total, byte| (total << 8) | u64::from(*byte))).ok()?;
    }
    if params.peek() == Some(0xa3) && Der::new(params.expect(0xa3).ok()?).uint().ok()? != [1] {
        return None;
    }
    (hash == mgf_hash).then_some((hash, salt))
}

/// `X509_verify(cert, key) == 1`: the signature over the TBS bytes checks out with `spki`.
pub(super) fn signature_valid(cert: &Certificate, spki: &[u8]) -> bool {
    let (Some(key), [0, signature @ ..]) = (public_key(spki), cert.signature) else { return false };
    let Some((needed, hash)) = algorithm_oid(cert.algorithm).as_deref().and_then(signature_scheme) else {
        return false;
    };
    if cert.algorithm != cert.tbs_algorithm || !key_fits(&key, needed) {
        return false;
    }
    let options = if needed == "rsa-pss" {
        let Some((hash, salt)) = pss_params(cert.algorithm) else { return false };
        SignOptions { hash, padding: rsa::PSS_PADDING, salt_length: Some(salt), p1363: false }
    } else {
        let padding = if needed == "rsa" { rsa::PKCS1_PADDING } else { 0 };
        SignOptions { hash, padding, salt_length: None, p1363: false }
    };
    ops::verify(&key, cert.tbs, signature, &options).unwrap_or(false)
}

/// `X509_check_private_key`: the PKCS#8 key's public half equals the certificate's key.
pub(super) fn private_key_matches(cert: &Certificate, pkcs8: &[u8]) -> bool {
    let private = Key::parse("der", "pkcs8", pkcs8, None);
    matches!((private, public_key(cert.spki)), (Ok(private), Some(public)) if private.spki() == public.spki())
}
