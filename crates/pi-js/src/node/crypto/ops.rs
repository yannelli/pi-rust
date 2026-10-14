//! Operations on asymmetric keys: generation, signatures, RSA encryption, and key agreement.

use num_bigint::BigUint;
use serde_json::Value;

use super::bigint::{from_hex, generate_prime, modp_group, random_below, to_fixed};
use super::der::{self, Der, fixed};
use super::dsa::Dsa;
use super::ec;
use super::keys::{Key, ec_private, okp_private};
use super::okp::Okp;
use super::random_bytes;
use super::rsa::{self, Rsa};
use crate::error::JsError;

pub(super) fn not_supported_for_key() -> JsError {
    JsError::new("error:1C8000A5:Provider routines::operation not supported for this keytype")
        .with_code("ERR_OSSL_EVP_OPERATION_NOT_SUPPORTED_FOR_THIS_KEYTYPE")
}

pub(super) fn generate(kind: &str, options: &Value) -> Result<Key, JsError> {
    let number = |name: &str| options.get(name).and_then(Value::as_u64);
    let text = |name: &str| options.get(name).and_then(Value::as_str).unwrap_or_default();
    match kind {
        "rsa" | "rsa-pss" => {
            let rsa = Rsa::generate(number("modulusLength").unwrap_or(0), number("publicExponent").unwrap_or(65537))?;
            Ok(Key::Rsa { pss: kind == "rsa-pss", rsa })
        }
        "ec" => {
            let curve = ec::curve(text("namedCurve")).ok_or_else(ec::invalid_curve)?;
            ec_private(curve, &curve.generate())
        }
        "dsa" => Ok(Key::Dsa(Dsa::generate(number("modulusLength").unwrap_or(0), number("divisorLength"))?)),
        "dh" => {
            let g = BigUint::from(number("generator").unwrap_or(2));
            let p = if !text("group").is_empty() {
                modp_group(text("group"))
                    .ok_or_else(|| JsError::new("Unknown DH group").with_code("ERR_CRYPTO_UNKNOWN_DH_GROUP"))?
            } else if !text("prime").is_empty() {
                from_hex(text("prime"))
            } else {
                generate_prime(number("primeLength").unwrap_or(0), true, None, None)?
            };
            let x = random_below(&(&p - 3u8))? + 2u8;
            Ok(Key::Dh { public: g.modpow(&x, &p), p, g, private: Some(x) })
        }
        other => match Okp::from_name(other) {
            Some(okp) => okp_private(okp, random_bytes(okp.size())?),
            None => Err(JsError::unsupported(&format!("crypto {other} keys"))),
        },
    }
}

/// DER `SEQUENCE { r, s }` from `r || s`, or the fixed-size IEEE P1363 form unchanged.
fn signature_encode(signature: Vec<u8>, p1363: bool) -> Vec<u8> {
    if p1363 {
        return signature;
    }
    let (r, s) = signature.split_at(signature.len() / 2);
    der::seq(&[der::uint(r), der::uint(s)])
}

/// `r || s` with `half`-byte halves from a DER or IEEE P1363 signature.
fn signature_decode(signature: &[u8], half: usize, p1363: bool) -> Option<Vec<u8>> {
    if p1363 {
        return (signature.len() == 2 * half).then(|| signature.to_vec());
    }
    let mut outer = Der::new(signature);
    let mut inner = outer.seq().ok()?;
    let (r, s) = (inner.uint().ok()?, inner.uint().ok()?);
    let fits = |value: &[u8]| fixed(value, 0).len() <= half;
    (inner.is_empty() && outer.is_empty() && fits(r) && fits(s)).then(|| [fixed(r, half), fixed(s, half)].concat())
}

/// Signature options; `padding == 0` and `salt_length == None` select Node's defaults.
pub(super) struct SignOptions<'a> {
    pub hash: &'a str,
    pub padding: i32,
    pub salt_length: Option<i32>,
    pub p1363: bool,
}

fn rsa_padding(pss: bool, padding: i32) -> Result<i32, JsError> {
    match padding {
        0 if pss => Ok(rsa::PSS_PADDING),
        0 => Ok(rsa::PKCS1_PADDING),
        rsa::PKCS1_PADDING if !pss => Ok(padding),
        rsa::PSS_PADDING => Ok(padding),
        _ => Err(JsError::new("error:1C8000A6:Provider routines::illegal or unsupported padding mode")
            .with_code("ERR_OSSL_ILLEGAL_OR_UNSUPPORTED_PADDING_MODE")),
    }
}

pub(super) fn sign(key: &Key, data: &[u8], options: &SignOptions) -> Result<Vec<u8>, JsError> {
    let hash = if options.hash.is_empty() { "sha256" } else { options.hash };
    match key {
        Key::Rsa { pss, rsa } => {
            let padding = rsa_padding(*pss, options.padding)?;
            rsa.sign(hash, data, padding, options.salt_length.unwrap_or(rsa::SALTLEN_MAX))
        }
        Key::Ec { curve, d: Some(d), .. } => {
            Ok(signature_encode(curve.sign(d, &super::digest(hash, data)?)?, options.p1363))
        }
        Key::Dsa(dsa) if dsa.x.is_some() => Ok(signature_encode(dsa.sign(&super::digest(hash, data)?)?, options.p1363)),
        Key::Okp { kind, private: Some(seed), .. } if kind.is_signing() => kind.sign(seed, data),
        _ => Err(not_supported_for_key()),
    }
}

pub(super) fn verify(key: &Key, data: &[u8], signature: &[u8], options: &SignOptions) -> Result<bool, JsError> {
    let hash = if options.hash.is_empty() { "sha256" } else { options.hash };
    match key {
        Key::Rsa { pss, rsa } => {
            let padding = rsa_padding(*pss, options.padding)?;
            rsa.verify(hash, data, signature, padding, options.salt_length.unwrap_or(rsa::SALTLEN_MAX))
        }
        Key::Ec { curve, point, .. } => {
            let hashed = super::digest(hash, data)?;
            Ok(signature_decode(signature, curve.size, options.p1363)
                .is_some_and(|signature| curve.verify(point, &hashed, &signature)))
        }
        Key::Dsa(dsa) => {
            let hashed = super::digest(hash, data)?;
            Ok(signature_decode(signature, dsa.scalar_size(), options.p1363)
                .is_some_and(|signature| dsa.verify(&hashed, &signature)))
        }
        Key::Okp { kind, public, .. } if kind.is_signing() => kind.verify(public, data, signature),
        _ => Err(not_supported_for_key()),
    }
}

/// `publicEncrypt`, `privateDecrypt`, `privateEncrypt`, or `publicDecrypt`.
pub(super) fn rsa_crypt(
    op: &str,
    key: &Key,
    data: &[u8],
    padding: i32,
    oaep_hash: &str,
    label: &[u8],
) -> Result<Vec<u8>, JsError> {
    let Key::Rsa { rsa, .. } = key else { return Err(not_supported_for_key()) };
    let oaep_hash = if oaep_hash.is_empty() { "sha1" } else { oaep_hash };
    match op {
        "publicEncrypt" => rsa.encrypt(false, data, padding, oaep_hash, label),
        "privateEncrypt" => rsa.encrypt(true, data, padding, oaep_hash, label),
        "privateDecrypt" => rsa.decrypt(true, data, padding, oaep_hash, label),
        _ => rsa.decrypt(false, data, padding, oaep_hash, label),
    }
}

/// `crypto.diffieHellman`: ECDH, X25519, or finite-field DH between two keys.
pub(super) fn agree(private: &Key, public: &Key) -> Result<Vec<u8>, JsError> {
    match (private, public) {
        (Key::Ec { curve, d: Some(d), .. }, Key::Ec { curve: other, point, .. }) if curve.name == other.name => {
            curve.ecdh(d, point)
        }
        (Key::Okp { kind, private: Some(secret), .. }, Key::Okp { kind: other, public, .. })
            if kind == other && !kind.is_signing() =>
        {
            kind.agree(secret, public)
        }
        (Key::Dh { p, g, private: Some(x), .. }, Key::Dh { p: other_p, g: other_g, public, .. })
            if p == other_p && g == other_g =>
        {
            Ok(to_fixed(&public.modpow(x, p), p.bits().div_ceil(8) as usize))
        }
        _ => Err(JsError::new(format!(
            "Incompatible key types for Diffie-Hellman: {} and {}",
            private.type_name(),
            public.type_name()
        ))
        .with_code("ERR_CRYPTO_INCOMPATIBLE_KEY")),
    }
}
