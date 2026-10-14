//! Operations on asymmetric keys: generation, signatures, RSA encryption, and key agreement.

use num_bigint::BigUint;
use ring::signature::{ED25519, Ed25519KeyPair, UnparsedPublicKey};
use serde_json::Value;

use super::bigint::{from_hex, generate_prime, modp_group, random_below, to_fixed};
use super::der::{self, Der};
use super::ec;
use super::keys::{Key, ec_private, okp_public};
use super::random_bytes;
use super::rsa::{self, Rsa};
use crate::error::JsError;

fn not_supported_for_key() -> JsError {
    JsError::new("error:1C8000A5:Provider routines::operation not supported for this keytype")
        .with_code("ERR_OSSL_EVP_OPERATION_NOT_SUPPORTED_FOR_THIS_KEYTYPE")
}

pub(super) fn generate(kind: &str, options: &Value) -> Result<Key, JsError> {
    let number = |name: &str| options.get(name).and_then(Value::as_u64);
    let text = |name: &str| options.get(name).and_then(Value::as_str).unwrap_or_default();
    match kind {
        "rsa" | "rsa-pss" => {
            let bits = number("modulusLength").unwrap_or(0);
            let e = BigUint::from(number("publicExponent").unwrap_or(65537));
            Ok(Key::Rsa { pss: kind == "rsa-pss", rsa: Rsa::generate(bits, &e)? })
        }
        "ec" => {
            let curve = ec::curve(text("namedCurve")).ok_or_else(ec::invalid_curve)?;
            ec_private(curve, curve.params().random_scalar()?)
        }
        "ed25519" | "x25519" => {
            let seed = random_bytes(32)?;
            let x25519 = kind == "x25519";
            Ok(Key::Okp { x25519, public: okp_public(x25519, &seed)?, private: Some(seed) })
        }
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
        other => Err(JsError::unsupported(&format!("crypto {other} keys"))),
    }
}

fn ecdsa_encode(curve: &ec::Params, (r, s): (BigUint, BigUint), p1363: bool) -> Vec<u8> {
    if p1363 {
        let size = curve.n.bits().div_ceil(8) as usize;
        [to_fixed(&r, size), to_fixed(&s, size)].concat()
    } else {
        der::seq(&[der::uint(&r.to_bytes_be()), der::uint(&s.to_bytes_be())])
    }
}

fn ecdsa_decode(curve: &ec::Params, signature: &[u8], p1363: bool) -> Option<(BigUint, BigUint)> {
    if p1363 {
        let size = curve.n.bits().div_ceil(8) as usize;
        return (signature.len() == 2 * size)
            .then(|| (BigUint::from_bytes_be(&signature[..size]), BigUint::from_bytes_be(&signature[size..])));
    }
    let mut outer = Der::new(signature);
    let mut inner = outer.seq().ok()?;
    let pair = (BigUint::from_bytes_be(inner.uint().ok()?), BigUint::from_bytes_be(inner.uint().ok()?));
    (inner.is_empty() && outer.is_empty()).then_some(pair)
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
            let params = curve.params();
            let signature = params.sign(d, &super::digest(hash, data)?)?;
            Ok(ecdsa_encode(&params, signature, options.p1363))
        }
        Key::Okp { x25519: false, private: Some(seed), .. } => {
            let pair = Ed25519KeyPair::from_seed_unchecked(seed).map_err(|_| der::unsupported_key())?;
            Ok(pair.sign(data).as_ref().to_vec())
        }
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
            let params = curve.params();
            let hashed = super::digest(hash, data)?;
            Ok(ecdsa_decode(&params, signature, options.p1363)
                .is_some_and(|(r, s)| params.verify(point, &hashed, &r, &s)))
        }
        Key::Okp { x25519: false, public, .. } => {
            Ok(UnparsedPublicKey::new(&ED25519, public).verify(data, signature).is_ok())
        }
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
            let params = curve.params();
            params.ecdh(d, &params.encode(point, 4))
        }
        (Key::Okp { x25519: true, private: Some(secret), .. }, Key::Okp { x25519: true, public, .. }) => {
            ec::x25519(secret, public)
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
