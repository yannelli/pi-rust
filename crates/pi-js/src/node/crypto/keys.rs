//! Asymmetric keys: PKCS#8, SPKI, PKCS#1, SEC1, X.509 and JWK import, plus export.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use num_bigint::BigUint;
use serde_json::{Map, Value, json};

use super::bigint::to_fixed;
use super::der::{self, Der, unsupported_key};
use super::ec::{self, Curve};
use super::rsa::{Rsa, RsaSecret};
use crate::error::JsError;

const RSA: &str = "1.2.840.113549.1.1.1";
const RSA_PSS: &str = "1.2.840.113549.1.1.10";
const EC: &str = "1.2.840.10045.2.1";
const ED25519: &str = "1.3.101.112";
const X25519: &str = "1.3.101.110";
const DH: &str = "1.2.840.113549.1.3.1";

pub(super) enum Key {
    Rsa {
        pss: bool,
        rsa: Rsa,
    },
    Ec {
        curve: &'static Curve,
        point: (BigUint, BigUint),
        d: Option<BigUint>,
    },
    /// Ed25519, or X25519 when `x25519` is set; `private` holds the 32-byte seed or scalar.
    Okp {
        x25519: bool,
        public: Vec<u8>,
        private: Option<Vec<u8>>,
    },
    Dh {
        p: BigUint,
        g: BigUint,
        public: BigUint,
        private: Option<BigUint>,
    },
}

fn big(bytes: &[u8]) -> BigUint {
    BigUint::from_bytes_be(bytes)
}

fn unsupported_type(oid: &str) -> JsError {
    let name = match oid {
        "1.3.101.113" => "ed448",
        "1.3.101.111" => "x448",
        "1.2.840.10040.4.1" => "dsa",
        _ => return unsupported_key(),
    };
    JsError::unsupported(&format!("crypto {name} keys"))
}

pub(super) fn okp_public(x25519: bool, private: &[u8]) -> Result<Vec<u8>, JsError> {
    if x25519 {
        return ec::x25519_public(private);
    }
    let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(private).map_err(|_| unsupported_key())?;
    Ok(ring::signature::KeyPair::public_key(&pair).as_ref().to_vec())
}

pub(super) fn ec_private(curve: &'static Curve, d: BigUint) -> Result<Key, JsError> {
    let params = curve.params();
    if d == BigUint::default() || d >= params.n {
        return Err(
            JsError::new("Private key is not valid for specified curve.").with_code("ERR_CRYPTO_INVALID_KEYTYPE")
        );
    }
    let point = params.mul_base(&d).ok_or_else(unsupported_key)?;
    Ok(Key::Ec { curve, point, d: Some(d) })
}

fn parse_pkcs1_private(data: &[u8]) -> Result<Rsa, JsError> {
    let mut der = Der::new(data).seq()?;
    der.uint()?;
    let mut next = || der.uint().map(big);
    let (n, e, d, p, q, dp, dq, qinv) = (next()?, next()?, next()?, next()?, next()?, next()?, next()?, next()?);
    Ok(Rsa { n, e, secret: Some(RsaSecret { d, p, q, dp, dq, qinv }) })
}

fn parse_pkcs1_public(data: &[u8]) -> Result<Rsa, JsError> {
    let mut der = Der::new(data).seq()?;
    Ok(Rsa { n: big(der.uint()?), e: big(der.uint()?), secret: None })
}

fn parse_sec1(data: &[u8], curve: Option<&'static Curve>) -> Result<Key, JsError> {
    let mut der = Der::new(data).seq()?;
    der.uint()?;
    let d = big(der.expect(0x04)?);
    let mut curve = curve;
    if der.peek() == Some(0xa0) {
        curve = ec::curve(&Der::new(der.expect(0xa0)?).oid()?);
    }
    ec_private(curve.ok_or_else(ec::invalid_curve)?, d)
}

fn parse_pkcs8(data: &[u8]) -> Result<Key, JsError> {
    let mut der = Der::new(data).seq()?;
    der.uint()?;
    let mut algorithm = der.seq()?;
    let oid = algorithm.oid()?;
    let private = der.expect(0x04)?;
    match oid.as_str() {
        RSA | RSA_PSS => Ok(Key::Rsa { pss: oid == RSA_PSS, rsa: parse_pkcs1_private(private)? }),
        EC => parse_sec1(private, Some(ec::curve(&algorithm.oid()?).ok_or_else(ec::invalid_curve)?)),
        ED25519 | X25519 => {
            let seed = Der::new(private).expect(0x04)?.to_vec();
            let x25519 = oid == X25519;
            Ok(Key::Okp { x25519, public: okp_public(x25519, &seed)?, private: Some(seed) })
        }
        DH => {
            let mut params = algorithm.seq()?;
            let (p, g) = (big(params.uint()?), big(params.uint()?));
            let x = big(Der::new(private).uint()?);
            Ok(Key::Dh { public: g.modpow(&x, &p), p, g, private: Some(x) })
        }
        other => Err(unsupported_type(other)),
    }
}

fn parse_spki(data: &[u8]) -> Result<Key, JsError> {
    let mut der = Der::new(data).seq()?;
    let mut algorithm = der.seq()?;
    let oid = algorithm.oid()?;
    let public = der.bits()?;
    match oid.as_str() {
        RSA | RSA_PSS => Ok(Key::Rsa { pss: oid == RSA_PSS, rsa: parse_pkcs1_public(public)? }),
        EC => {
            let curve = ec::curve(&algorithm.oid()?).ok_or_else(ec::invalid_curve)?;
            Ok(Key::Ec { curve, point: curve.params().decode(public)?, d: None })
        }
        ED25519 | X25519 if public.len() == 32 => {
            Ok(Key::Okp { x25519: oid == X25519, public: public.to_vec(), private: None })
        }
        DH => {
            let mut params = algorithm.seq()?;
            let (p, g) = (big(params.uint()?), big(params.uint()?));
            Ok(Key::Dh { p, g, public: big(Der::new(public).uint()?), private: None })
        }
        other => Err(unsupported_type(other)),
    }
}

/// The SubjectPublicKeyInfo inside an X.509 certificate.
fn parse_certificate(data: &[u8]) -> Result<Key, JsError> {
    let mut tbs = Der::new(data).seq()?.seq()?;
    if tbs.peek() == Some(0xa0) {
        tbs.read()?;
    }
    for _ in 0..5 {
        tbs.read()?;
    }
    parse_spki(&der::tlv(0x30, tbs.expect(0x30)?))
}

impl Key {
    /// Parse `data` given as `pem`, `der` (with `der_type`), or `jwk` (JSON text).
    pub(super) fn parse(format: &str, der_type: &str, data: &[u8]) -> Result<Self, JsError> {
        match format {
            "jwk" => Self::from_jwk(&serde_json::from_slice(data).map_err(|_| invalid_jwk())?),
            "der" => match der_type {
                "pkcs8" => parse_pkcs8(data),
                "spki" => parse_spki(data),
                "sec1" => parse_sec1(data, None),
                "pkcs1" => parse_pkcs1_private(data)
                    .map(|rsa| Key::Rsa { pss: false, rsa })
                    .or_else(|_| parse_pkcs1_public(data).map(|rsa| Key::Rsa { pss: false, rsa })),
                _ => parse_pkcs8(data).or_else(|_| parse_spki(data)),
            },
            _ => {
                let text = String::from_utf8_lossy(data);
                let (label, der) = der::pem_decode(&text)?;
                match label.as_str() {
                    "PRIVATE KEY" => parse_pkcs8(&der),
                    "PUBLIC KEY" => parse_spki(&der),
                    "RSA PRIVATE KEY" => Ok(Key::Rsa { pss: false, rsa: parse_pkcs1_private(&der)? }),
                    "RSA PUBLIC KEY" => Ok(Key::Rsa { pss: false, rsa: parse_pkcs1_public(&der)? }),
                    "EC PRIVATE KEY" => parse_sec1(&der, None),
                    "CERTIFICATE" => parse_certificate(&der),
                    _ => Err(unsupported_key()),
                }
            }
        }
    }

    pub(super) fn is_private(&self) -> bool {
        match self {
            Key::Rsa { rsa, .. } => rsa.secret.is_some(),
            Key::Ec { d, .. } => d.is_some(),
            Key::Okp { private, .. } => private.is_some(),
            Key::Dh { private, .. } => private.is_some(),
        }
    }

    pub(super) fn type_name(&self) -> &'static str {
        match self {
            Key::Rsa { pss: true, .. } => "rsa-pss",
            Key::Rsa { .. } => "rsa",
            Key::Ec { .. } => "ec",
            Key::Okp { x25519: true, .. } => "x25519",
            Key::Okp { .. } => "ed25519",
            Key::Dh { .. } => "dh",
        }
    }

    pub(super) fn details(&self) -> Value {
        match self {
            Key::Rsa { rsa, .. } => json!({ "modulusLength": rsa.n.bits(), "publicExponent": rsa.e.to_string() }),
            Key::Ec { curve, .. } => json!({ "namedCurve": curve.name }),
            _ => json!({}),
        }
    }

    fn algorithm(&self) -> Vec<u8> {
        match self {
            Key::Rsa { pss: true, .. } => der::seq(&[der::oid(RSA_PSS)]),
            Key::Rsa { .. } => der::seq(&[der::oid(RSA), der::null()]),
            Key::Ec { curve, .. } => der::seq(&[der::oid(EC), der::oid(curve.oid)]),
            Key::Okp { x25519, .. } => der::seq(&[der::oid(if *x25519 { X25519 } else { ED25519 })]),
            Key::Dh { p, g, .. } => {
                der::seq(&[der::oid(DH), der::seq(&[der::uint(&p.to_bytes_be()), der::uint(&g.to_bytes_be())])])
            }
        }
    }

    fn pkcs1(&self, private: bool) -> Result<Vec<u8>, JsError> {
        let Key::Rsa { rsa, .. } = self else { return Err(incompatible("pkcs1", "RSA")) };
        let int = |value: &BigUint| der::uint(&value.to_bytes_be());
        match (&rsa.secret, private) {
            (Some(s), true) => Ok(der::seq(&[
                der::uint(&[0]),
                int(&rsa.n),
                int(&rsa.e),
                int(&s.d),
                int(&s.p),
                int(&s.q),
                int(&s.dp),
                int(&s.dq),
                int(&s.qinv),
            ])),
            _ => Ok(der::seq(&[int(&rsa.n), int(&rsa.e)])),
        }
    }

    fn sec1(&self, with_params: bool) -> Result<Vec<u8>, JsError> {
        let Key::Ec { curve, point, d: Some(d) } = self else { return Err(incompatible("sec1", "EC")) };
        let params = curve.params();
        let mut parts = vec![der::uint(&[1]), der::octets(&to_fixed(d, params.n.bits().div_ceil(8) as usize))];
        if with_params {
            parts.push(der::tlv(0xa0, &der::oid(curve.oid)));
        }
        parts.push(der::tlv(0xa1, &der::bits(&params.encode(point, 4))));
        Ok(der::seq(&parts))
    }

    pub(super) fn pkcs8(&self) -> Result<Vec<u8>, JsError> {
        let private = match self {
            Key::Rsa { .. } => self.pkcs1(true)?,
            Key::Ec { .. } => self.sec1(false)?,
            Key::Okp { private: Some(seed), .. } => der::octets(seed),
            Key::Dh { private: Some(x), .. } => der::uint(&x.to_bytes_be()),
            _ => return Err(unsupported_key()),
        };
        Ok(der::seq(&[der::uint(&[0]), self.algorithm(), der::octets(&private)]))
    }

    pub(super) fn spki(&self) -> Vec<u8> {
        let public = match self {
            Key::Rsa { rsa, .. } => der::seq(&[der::uint(&rsa.n.to_bytes_be()), der::uint(&rsa.e.to_bytes_be())]),
            Key::Ec { curve, point, .. } => curve.params().encode(point, 4),
            Key::Okp { public, .. } => public.clone(),
            Key::Dh { public, .. } => der::uint(&public.to_bytes_be()),
        };
        der::seq(&[self.algorithm(), der::bits(&public)])
    }

    /// Export as `pem` or `der` with an encoding `der_type`, or as `jwk` JSON text.
    pub(super) fn export(&self, private: bool, format: &str, der_type: &str) -> Result<Vec<u8>, JsError> {
        if format == "jwk" {
            return Ok(self.to_jwk(private)?.to_string().into_bytes());
        }
        let (label, der) = match (private, der_type) {
            (true, "pkcs1") => ("RSA PRIVATE KEY", self.pkcs1(true)?),
            (true, "sec1") => ("EC PRIVATE KEY", self.sec1(true)?),
            (true, _) => ("PRIVATE KEY", self.pkcs8()?),
            (false, "pkcs1") => ("RSA PUBLIC KEY", self.pkcs1(false)?),
            (false, _) => ("PUBLIC KEY", self.spki()),
        };
        Ok(if format == "der" { der } else { der::pem_encode(label, &der).into_bytes() })
    }

    fn to_jwk(&self, private: bool) -> Result<Value, JsError> {
        let b64 = |bytes: &[u8]| Value::String(URL_SAFE_NO_PAD.encode(bytes));
        let num = |value: &BigUint| b64(&value.to_bytes_be());
        let mut jwk = Map::new();
        match self {
            Key::Rsa { pss: false, rsa } => {
                jwk.insert("kty".into(), "RSA".into());
                jwk.insert("n".into(), num(&rsa.n));
                jwk.insert("e".into(), num(&rsa.e));
                if let (Some(s), true) = (&rsa.secret, private) {
                    for (name, value) in
                        [("d", &s.d), ("p", &s.p), ("q", &s.q), ("dp", &s.dp), ("dq", &s.dq), ("qi", &s.qinv)]
                    {
                        jwk.insert(name.into(), num(value));
                    }
                }
            }
            Key::Ec { curve, point, d } => {
                let size = curve.params().size;
                jwk.insert("kty".into(), "EC".into());
                jwk.insert("crv".into(), curve.jwk.into());
                jwk.insert("x".into(), b64(&to_fixed(&point.0, size)));
                jwk.insert("y".into(), b64(&to_fixed(&point.1, size)));
                if let (Some(d), true) = (d, private) {
                    jwk.insert("d".into(), b64(&to_fixed(d, curve.params().n.bits().div_ceil(8) as usize)));
                }
            }
            Key::Okp { x25519, public, private: secret } => {
                jwk.insert("crv".into(), (if *x25519 { "X25519" } else { "Ed25519" }).into());
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

    fn from_jwk(jwk: &Value) -> Result<Self, JsError> {
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
                let (n, e) = (big(&required("n")?), big(&required("e")?));
                let secret = match field("d")? {
                    Some(d) => {
                        let next = |name| required(name).map(|bytes| big(&bytes));
                        Some(RsaSecret {
                            d: big(&d),
                            p: next("p")?,
                            q: next("q")?,
                            dp: next("dp")?,
                            dq: next("dq")?,
                            qinv: next("qi")?,
                        })
                    }
                    None => None,
                };
                Ok(Key::Rsa { pss: false, rsa: Rsa { n, e, secret } })
            }
            Some("EC") => {
                let curve = ec::curve(crv).filter(|curve| curve.jwk == crv).ok_or_else(|| {
                    JsError::new(format!("Unsupported JWK EC curve: {crv}."))
                        .with_code("ERR_CRYPTO_JWK_UNSUPPORTED_CURVE")
                })?;
                if let Some(d) = field("d")? {
                    return ec_private(curve, big(&d));
                }
                let params = curve.params();
                let point = params.decode(
                    &[
                        vec![4],
                        to_fixed(&big(&required("x")?), params.size),
                        to_fixed(&big(&required("y")?), params.size),
                    ]
                    .concat(),
                )?;
                Ok(Key::Ec { curve, point, d: None })
            }
            Some("OKP") => {
                let x25519 = match crv {
                    "Ed25519" => false,
                    "X25519" => true,
                    "Ed448" | "X448" => return Err(JsError::unsupported(&format!("crypto {crv} keys"))),
                    _ => {
                        return Err(JsError::new(format!("Unsupported JWK OKP curve: {crv}."))
                            .with_code("ERR_CRYPTO_JWK_UNSUPPORTED_CURVE"));
                    }
                };
                let public = required("x")?;
                match field("d")? {
                    Some(secret) if secret.len() == 32 => {
                        Ok(Key::Okp { x25519, public: okp_public(x25519, &secret)?, private: Some(secret) })
                    }
                    None if public.len() == 32 => Ok(Key::Okp { x25519, public, private: None }),
                    _ => Err(invalid_jwk()),
                }
            }
            _ => Err(JsError::type_error("Invalid JWK data").with_code("ERR_CRYPTO_INVALID_JWK")),
        }
    }
}

fn invalid_jwk() -> JsError {
    JsError::type_error("Invalid JWK data").with_code("ERR_CRYPTO_INVALID_JWK")
}

fn incompatible(encoding: &str, kind: &str) -> JsError {
    JsError::type_error(format!("The selected key encoding {encoding} can only be used for {kind} keys."))
        .with_code("ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS")
}
