//! JSON Web Key import and export for RSA, EC, and OKP keys.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value};

use super::der::fixed;
use super::ec;
use super::keys::{Key, ec_private, okp_private};
use super::okp::Okp;
use super::rsa::Rsa;
use crate::error::JsError;

pub(super) fn invalid_jwk() -> JsError {
    JsError::type_error("Invalid JWK data").with_code("ERR_CRYPTO_INVALID_JWK")
}

pub(super) fn to_jwk(key: &Key, private: bool) -> Result<Value, JsError> {
    let b64 = |bytes: &[u8]| Value::String(URL_SAFE_NO_PAD.encode(bytes));
    let mut jwk = Map::new();
    match key {
        Key::Rsa { pss: false, rsa } => {
            jwk.insert("kty".into(), "RSA".into());
            jwk.insert("n".into(), b64(&rsa.n()));
            jwk.insert("e".into(), b64(&rsa.e()));
            if let (Some(parts), true) = (rsa.private_parts(), private) {
                for (name, value) in ["d", "p", "q", "dp", "dq", "qi"].into_iter().zip(&parts[2..]) {
                    jwk.insert(name.into(), b64(value));
                }
            }
        }
        Key::Ec { curve, point, d } => {
            jwk.insert("kty".into(), "EC".into());
            jwk.insert("crv".into(), curve.jwk.into());
            jwk.insert("x".into(), b64(&point[1..=curve.size]));
            jwk.insert("y".into(), b64(&point[1 + curve.size..]));
            if let (Some(d), true) = (d, private) {
                jwk.insert("d".into(), b64(d));
            }
        }
        Key::Okp { kind, public, private: secret } => {
            jwk.insert("crv".into(), kind.jwk().into());
            if let (Some(secret), true) = (secret, private) {
                jwk.insert("d".into(), b64(secret));
            }
            jwk.insert("x".into(), b64(public));
            jwk.insert("kty".into(), "OKP".into());
        }
        _ => {
            return Err(
                JsError::type_error("Unsupported JWK Key Type.").with_code("ERR_CRYPTO_JWK_UNSUPPORTED_KEY_TYPE")
            );
        }
    }
    Ok(Value::Object(jwk))
}

fn unsupported_curve(kty: &str, crv: &str) -> JsError {
    JsError::new(format!("Unsupported JWK {kty} curve: {crv}.")).with_code("ERR_CRYPTO_JWK_UNSUPPORTED_CURVE")
}

pub(super) fn from_jwk(jwk: &Value) -> Result<Key, JsError> {
    let field = |name: &str| -> Result<Option<Vec<u8>>, JsError> {
        match jwk.get(name) {
            None => Ok(None),
            Some(Value::String(text)) => {
                URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')).map(Some).map_err(|_| invalid_jwk())
            }
            Some(_) => Err(invalid_jwk()),
        }
    };
    let required = |name: &str| field(name)?.ok_or_else(invalid_jwk);
    let crv = jwk.get("crv").and_then(Value::as_str).unwrap_or_default();
    match jwk.get("kty").and_then(Value::as_str) {
        Some("RSA") => {
            let (n, e) = (required("n")?, required("e")?);
            let rsa = match field("d")? {
                Some(d) => Rsa::from_private(&n, &e, &d, &required("p")?, &required("q")?)?,
                None => Rsa::from_public(&n, &e)?,
            };
            Ok(Key::Rsa { pss: false, rsa })
        }
        Some("EC") => {
            let curve = ec::curve(crv).filter(|curve| curve.jwk == crv).ok_or_else(|| unsupported_curve("EC", crv))?;
            if let Some(d) = field("d")? {
                return ec_private(curve, &d);
            }
            let point = [vec![4], fixed(&required("x")?, curve.size), fixed(&required("y")?, curve.size)].concat();
            Ok(Key::Ec { curve, point: curve.convert(&point, 4)?, d: None })
        }
        Some("OKP") => {
            let kind =
                Okp::from_name(crv).filter(|kind| kind.jwk() == crv).ok_or_else(|| unsupported_curve("OKP", crv))?;
            let public = required("x")?;
            match field("d")? {
                Some(secret) if secret.len() == kind.size() => okp_private(kind, secret),
                None if public.len() == kind.size() => Ok(Key::Okp { kind, public, private: None }),
                _ => Err(invalid_jwk()),
            }
        }
        _ => Err(invalid_jwk()),
    }
}
