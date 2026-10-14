//! Asymmetric keys: PKCS#8 (plain or encrypted), SPKI, PKCS#1, SEC1, X.509 and JWK import, plus export.

use num_bigint::BigUint;
use serde_json::{Value, json};

use super::der::{self, Der, fixed, unsupported_key};
use super::dsa::Dsa;
use super::ec::{self, Curve};
use super::keyenc;
use super::okp::Okp;
use super::rsa::{Rsa, pkcs1_private_der};
use crate::error::JsError;

const RSA: &str = "1.2.840.113549.1.1.1";
const RSA_PSS: &str = "1.2.840.113549.1.1.10";
const EC: &str = "1.2.840.10045.2.1";
const DH: &str = "1.2.840.113549.1.3.1";
const DSA: &str = "1.2.840.10040.4.1";

pub(super) enum Key {
    Rsa {
        pss: bool,
        rsa: Rsa,
    },
    /// `point` is the uncompressed SEC1 point; `d` the fixed-size private scalar.
    Ec {
        curve: &'static Curve,
        point: Vec<u8>,
        d: Option<Vec<u8>>,
    },
    /// Ed25519, X25519, Ed448, or X448; `private` holds the seed or scalar.
    Okp {
        kind: Okp,
        public: Vec<u8>,
        private: Option<Vec<u8>>,
    },
    Dsa(Dsa),
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

pub(super) fn ec_private(curve: &'static Curve, d: &[u8]) -> Result<Key, JsError> {
    let d = fixed(d, curve.size);
    Ok(Key::Ec { curve, point: curve.public(&d)?, d: Some(d) })
}

pub(super) fn okp_private(kind: Okp, private: Vec<u8>) -> Result<Key, JsError> {
    Ok(Key::Okp { kind, public: kind.public(&private)?, private: Some(private) })
}

fn parse_pkcs1_private(data: &[u8]) -> Result<Rsa, JsError> {
    let mut der = Der::new(data).seq()?;
    der.uint()?;
    let mut next = || der.uint();
    let (n, e, d, p, q) = (next()?, next()?, next()?, next()?, next()?);
    Rsa::from_private(n, e, d, p, q)
}

fn parse_pkcs1_public(data: &[u8]) -> Result<Rsa, JsError> {
    let mut der = Der::new(data).seq()?;
    Rsa::from_public(der.uint()?, der.uint()?)
}

fn parse_sec1(data: &[u8], curve: Option<&'static Curve>) -> Result<Key, JsError> {
    let mut der = Der::new(data).seq()?;
    der.uint()?;
    let d = der.expect(0x04)?;
    let mut curve = curve;
    if der.peek() == Some(0xa0) {
        curve = ec::curve(&Der::new(der.expect(0xa0)?).oid()?);
    }
    ec_private(curve.ok_or_else(ec::invalid_curve)?, d)
}

fn dsa_params(algorithm: &mut Der) -> Result<[Vec<u8>; 3], JsError> {
    let mut params = algorithm.seq()?;
    Ok([params.uint()?.to_vec(), params.uint()?.to_vec(), params.uint()?.to_vec()])
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
        DH => {
            let mut params = algorithm.seq()?;
            let (p, g) = (big(params.uint()?), big(params.uint()?));
            let x = big(Der::new(private).uint()?);
            Ok(Key::Dh { public: g.modpow(&x, &p), p, g, private: Some(x) })
        }
        DSA => {
            let [p, q, g] = dsa_params(&mut algorithm)?;
            Ok(Key::Dsa(Dsa::from_private(&p, &q, &g, Der::new(private).uint()?)?))
        }
        other => match Okp::from_oid(other) {
            Some(kind) => okp_private(kind, Der::new(private).expect(0x04)?.to_vec()),
            None => Err(unsupported_key()),
        },
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
            Ok(Key::Ec { curve, point: curve.convert(public, 4)?, d: None })
        }
        DH => {
            let mut params = algorithm.seq()?;
            let (p, g) = (big(params.uint()?), big(params.uint()?));
            Ok(Key::Dh { p, g, public: big(Der::new(public).uint()?), private: None })
        }
        DSA => {
            let [p, q, g] = dsa_params(&mut algorithm)?;
            Ok(Key::Dsa(Dsa { p, q, g, y: Der::new(public).uint()?.to_vec(), x: None }))
        }
        other => match Okp::from_oid(other) {
            Some(kind) if public.len() == kind.size() => Ok(Key::Okp { kind, public: public.to_vec(), private: None }),
            _ => Err(unsupported_key()),
        },
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

fn parse_pem(data: &[u8], passphrase: Option<&[u8]>) -> Result<Key, JsError> {
    let pem = der::pem_decode(&String::from_utf8_lossy(data))?;
    let dek = pem.headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("DEK-Info"));
    let der = match dek {
        Some((_, value)) => keyenc::decrypt_legacy(value, &pem.der, passphrase)?,
        None => pem.der,
    };
    match pem.label.as_str() {
        "PRIVATE KEY" => parse_pkcs8(&der),
        "ENCRYPTED PRIVATE KEY" => parse_pkcs8(&keyenc::decrypt_pkcs8(&der, passphrase)?),
        "PUBLIC KEY" => parse_spki(&der),
        "RSA PRIVATE KEY" => Ok(Key::Rsa { pss: false, rsa: parse_pkcs1_private(&der)? }),
        "RSA PUBLIC KEY" => Ok(Key::Rsa { pss: false, rsa: parse_pkcs1_public(&der)? }),
        "EC PRIVATE KEY" => parse_sec1(&der, None),
        "CERTIFICATE" => parse_certificate(&der),
        _ => Err(unsupported_key()),
    }
}

impl Key {
    /// Parse `data` given as `pem`, `der` (with `der_type`), or `jwk` (JSON text).
    pub(super) fn parse(format: &str, der_type: &str, data: &[u8], passphrase: Option<&[u8]>) -> Result<Self, JsError> {
        match format {
            "jwk" => super::jwk::from_jwk(&serde_json::from_slice(data).map_err(|_| super::jwk::invalid_jwk())?),
            "der" => match der_type {
                "pkcs8" if keyenc::is_encrypted_pkcs8(data) => parse_pkcs8(&keyenc::decrypt_pkcs8(data, passphrase)?),
                "pkcs8" => parse_pkcs8(data),
                "spki" => parse_spki(data),
                "sec1" => parse_sec1(data, None),
                "pkcs1" => parse_pkcs1_private(data)
                    .or_else(|_| parse_pkcs1_public(data))
                    .map(|rsa| Key::Rsa { pss: false, rsa }),
                _ => parse_pkcs8(data).or_else(|_| parse_spki(data)),
            },
            _ => parse_pem(data, passphrase),
        }
    }

    pub(super) fn is_private(&self) -> bool {
        match self {
            Key::Rsa { rsa, .. } => rsa.private.is_some(),
            Key::Ec { d, .. } => d.is_some(),
            Key::Okp { private, .. } => private.is_some(),
            Key::Dsa(dsa) => dsa.x.is_some(),
            Key::Dh { private, .. } => private.is_some(),
        }
    }

    pub(super) fn type_name(&self) -> &'static str {
        match self {
            Key::Rsa { pss: true, .. } => "rsa-pss",
            Key::Rsa { .. } => "rsa",
            Key::Ec { .. } => "ec",
            Key::Okp { kind, .. } => kind.name(),
            Key::Dsa(_) => "dsa",
            Key::Dh { .. } => "dh",
        }
    }

    pub(super) fn details(&self) -> Value {
        match self {
            Key::Rsa { rsa, .. } => json!({ "modulusLength": rsa.bits(), "publicExponent": rsa.exponent() }),
            Key::Ec { curve, .. } => json!({ "namedCurve": curve.name }),
            Key::Dsa(dsa) => json!({ "modulusLength": dsa.modulus_bits(), "divisorLength": dsa.divisor_bits() }),
            _ => json!({}),
        }
    }

    fn algorithm(&self) -> Vec<u8> {
        match self {
            Key::Rsa { pss: true, .. } => der::seq(&[der::oid(RSA_PSS)]),
            Key::Rsa { .. } => der::seq(&[der::oid(RSA), der::null()]),
            Key::Ec { curve, .. } => der::seq(&[der::oid(EC), der::oid(curve.oid)]),
            Key::Okp { kind, .. } => der::seq(&[der::oid(kind.oid())]),
            Key::Dsa(dsa) => {
                der::seq(&[der::oid(DSA), der::seq(&[der::uint(&dsa.p), der::uint(&dsa.q), der::uint(&dsa.g)])])
            }
            Key::Dh { p, g, .. } => {
                der::seq(&[der::oid(DH), der::seq(&[der::uint(&p.to_bytes_be()), der::uint(&g.to_bytes_be())])])
            }
        }
    }

    fn pkcs1(&self, private: bool) -> Result<Vec<u8>, JsError> {
        let Key::Rsa { rsa, .. } = self else { return Err(incompatible("pkcs1", "RSA")) };
        match (rsa.private_parts(), private) {
            (Some(parts), true) => Ok(pkcs1_private_der(&parts)),
            _ => Ok(der::seq(&[der::uint(&rsa.n()), der::uint(&rsa.e())])),
        }
    }

    fn sec1(&self, with_params: bool) -> Result<Vec<u8>, JsError> {
        let Key::Ec { curve, point, d: Some(d) } = self else { return Err(incompatible("sec1", "EC")) };
        let mut parts = vec![der::uint(&[1]), der::octets(d)];
        if with_params {
            parts.push(der::tlv(0xa0, &der::oid(curve.oid)));
        }
        parts.push(der::tlv(0xa1, &der::bits(point)));
        Ok(der::seq(&parts))
    }

    pub(super) fn pkcs8(&self) -> Result<Vec<u8>, JsError> {
        let private = match self {
            Key::Rsa { .. } => self.pkcs1(true)?,
            Key::Ec { .. } => self.sec1(false)?,
            Key::Okp { private: Some(seed), .. } => der::octets(seed),
            Key::Dsa(Dsa { x: Some(x), .. }) => der::uint(x),
            Key::Dh { private: Some(x), .. } => der::uint(&x.to_bytes_be()),
            _ => return Err(unsupported_key()),
        };
        Ok(der::seq(&[der::uint(&[0]), self.algorithm(), der::octets(&private)]))
    }

    pub(super) fn spki(&self) -> Vec<u8> {
        let public = match self {
            Key::Rsa { rsa, .. } => der::seq(&[der::uint(&rsa.n()), der::uint(&rsa.e())]),
            Key::Ec { point, .. } => point.clone(),
            Key::Okp { public, .. } => public.clone(),
            Key::Dsa(dsa) => der::uint(&dsa.y),
            Key::Dh { public, .. } => der::uint(&public.to_bytes_be()),
        };
        der::seq(&[self.algorithm(), der::bits(&public)])
    }

    /// Export as `pem` or `der` with an encoding `der_type`, or as `jwk` JSON text. A `cipher`
    /// and passphrase encrypt private keys: PBES2 for PKCS#8, legacy PEM headers otherwise.
    pub(super) fn export(
        &self,
        private: bool,
        format: &str,
        der_type: &str,
        cipher: Option<(&str, &[u8])>,
    ) -> Result<Vec<u8>, JsError> {
        if format == "jwk" {
            return Ok(super::jwk::to_jwk(self, private)?.to_string().into_bytes());
        }
        let (label, der) = match (private, der_type) {
            (true, "pkcs1") => ("RSA PRIVATE KEY", self.pkcs1(true)?),
            (true, "sec1") => ("EC PRIVATE KEY", self.sec1(true)?),
            (true, _) => ("PRIVATE KEY", self.pkcs8()?),
            (false, "pkcs1") => ("RSA PUBLIC KEY", self.pkcs1(false)?),
            (false, _) => ("PUBLIC KEY", self.spki()),
        };
        let (label, der) = match (cipher, label) {
            (Some((cipher, passphrase)), "PRIVATE KEY") => {
                ("ENCRYPTED PRIVATE KEY", keyenc::encrypt_pkcs8(&der, cipher, passphrase)?)
            }
            (Some((cipher, passphrase)), _) if private && format != "der" => {
                return Ok(keyenc::encrypt_legacy(label, &der, cipher, passphrase)?.into_bytes());
            }
            (Some(_), _) if private => {
                return Err(JsError::type_error(format!(
                    "The selected key encoding {der_type} does not support encryption."
                ))
                .with_code("ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS"));
            }
            _ => (label, der),
        };
        Ok(if format == "der" { der } else { der::pem_encode(label, &der).into_bytes() })
    }
}

pub(super) fn incompatible(encoding: &str, kind: &str) -> JsError {
    JsError::type_error(format!("The selected key encoding {encoding} can only be used for {kind} keys."))
        .with_code("ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS")
}
