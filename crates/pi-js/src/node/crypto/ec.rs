//! ECDSA and ECDH on P-256, P-384, P-521 and secp256k1 over the RustCrypto curve crates.
//! Points travel as SEC1 bytes and private scalars as fixed-size big-endian bytes.

use p256::elliptic_curve::Generate;
use p256::elliptic_curve::sec1::ToSec1Point;

use super::der::fixed;
use crate::error::JsError;

#[derive(Clone, Copy)]
enum Id {
    P256,
    P384,
    P521,
    K256,
}

pub(super) struct Curve {
    /// Node's name (`getCurves`), the JWK name, and the OID.
    pub name: &'static str,
    pub jwk: &'static str,
    pub oid: &'static str,
    /// Byte length of a field element and of a private scalar.
    pub size: usize,
    id: Id,
}

pub(super) const CURVES: &[Curve] = &[
    Curve { name: "prime256v1", jwk: "P-256", oid: "1.2.840.10045.3.1.7", size: 32, id: Id::P256 },
    Curve { name: "secp384r1", jwk: "P-384", oid: "1.3.132.0.34", size: 48, id: Id::P384 },
    Curve { name: "secp521r1", jwk: "P-521", oid: "1.3.132.0.35", size: 66, id: Id::P521 },
    Curve { name: "secp256k1", jwk: "secp256k1", oid: "1.3.132.0.10", size: 32, id: Id::K256 },
];

/// Run `$body` with `$c` bound to the curve crate for `$curve`.
macro_rules! dispatch {
    ($curve:expr, $c:ident => $body:expr) => {
        match $curve.id {
            Id::P256 => {
                use p256 as $c;
                $body
            }
            Id::P384 => {
                use p384 as $c;
                $body
            }
            Id::P521 => {
                use p521 as $c;
                $body
            }
            Id::K256 => {
                use k256 as $c;
                $body
            }
        }
    };
}

/// Find a curve by Node name, alias, JWK name, or OID.
pub(super) fn curve(name: &str) -> Option<&'static Curve> {
    let name = match name {
        "P-256" | "secp256r1" => "prime256v1",
        "P-384" => "secp384r1",
        "P-521" => "secp521r1",
        other => other,
    };
    CURVES.iter().find(|curve| curve.name == name || curve.jwk == name || curve.oid == name)
}

pub(super) fn invalid_curve() -> JsError {
    JsError::type_error("Invalid EC curve name").with_code("ERR_CRYPTO_INVALID_CURVE")
}

fn invalid_point() -> JsError {
    JsError::new("Public key is not valid for specified curve").with_code("ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY")
}

fn invalid_private() -> JsError {
    JsError::new("Private key is not valid for specified curve.").with_code("ERR_CRYPTO_INVALID_KEYTYPE")
}

impl Curve {
    /// Hybrid points (prefix 6 or 7) as uncompressed ones, after checking the parity bit.
    fn unhybrid(&self, bytes: &[u8]) -> Result<Vec<u8>, JsError> {
        match bytes.first() {
            Some(&tag @ (6 | 7)) if bytes.len() == 1 + 2 * self.size => {
                if (bytes[bytes.len() - 1] & 1) != (tag & 1) {
                    return Err(invalid_point());
                }
                Ok([&[4], &bytes[1..]].concat())
            }
            _ => Ok(bytes.to_vec()),
        }
    }

    /// A random private scalar.
    pub(super) fn generate(&self) -> Vec<u8> {
        dispatch!(self, c => c::SecretKey::generate().to_bytes().to_vec())
    }

    /// The uncompressed public point for a private scalar.
    pub(super) fn public(&self, private: &[u8]) -> Result<Vec<u8>, JsError> {
        dispatch!(self, c => {
            let key = c::SecretKey::from_slice(&fixed(private, self.size)).map_err(|_| invalid_private())?;
            Ok(key.public_key().to_sec1_point(false).as_bytes().to_vec())
        })
    }

    /// Re-encode a point as compressed (2), uncompressed (4), or hybrid (6) bytes.
    pub(super) fn convert(&self, point: &[u8], form: u8) -> Result<Vec<u8>, JsError> {
        let point = self.unhybrid(point)?;
        let mut out = dispatch!(self, c => {
            let key = c::PublicKey::from_sec1_bytes(&point).map_err(|_| invalid_point())?;
            key.to_sec1_point(form == 2).as_bytes().to_vec()
        });
        if form == 6 {
            out[0] = 6 | (out[out.len() - 1] & 1);
        }
        Ok(out)
    }

    /// The x coordinate of `private * public`.
    pub(super) fn ecdh(&self, private: &[u8], public: &[u8]) -> Result<Vec<u8>, JsError> {
        let public = self.unhybrid(public)?;
        dispatch!(self, c => {
            let key = c::SecretKey::from_slice(&fixed(private, self.size)).map_err(|_| invalid_private())?;
            let point = c::PublicKey::from_sec1_bytes(&public).map_err(|_| invalid_point())?;
            let shared = c::elliptic_curve::ecdh::diffie_hellman(key.to_nonzero_scalar(), point.as_affine());
            Ok(shared.raw_secret_bytes().to_vec())
        })
    }

    /// An ECDSA signature `r || s` over a message digest.
    pub(super) fn sign(&self, private: &[u8], digest: &[u8]) -> Result<Vec<u8>, JsError> {
        use p256::ecdsa::signature::hazmat::PrehashSigner;
        dispatch!(self, c => {
            let key = c::ecdsa::SigningKey::from_slice(&fixed(private, self.size)).map_err(|_| invalid_private())?;
            let signature: c::ecdsa::Signature = key
                .sign_prehash(digest)
                .map_err(|_| JsError::new("ECDSA signing failed").with_code("ERR_CRYPTO_OPERATION_FAILED"))?;
            Ok(signature.to_bytes().to_vec())
        })
    }

    /// Check an ECDSA signature `r || s`; high-S values are accepted, as OpenSSL does.
    pub(super) fn verify(&self, public: &[u8], digest: &[u8], signature: &[u8]) -> bool {
        use p256::ecdsa::signature::hazmat::PrehashVerifier;
        dispatch!(self, c => {
            let (Ok(key), Ok(signature)) =
                (c::ecdsa::VerifyingKey::from_sec1_bytes(public), c::ecdsa::Signature::from_slice(signature))
            else {
                return false;
            };
            key.verify_prehash(digest, &signature.normalize_s()).is_ok()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn curves_generate_sign_and_agree() {
        for curve in CURVES {
            let (a, b) = (curve.generate(), curve.generate());
            let (pa, pb) = (curve.public(&a).unwrap(), curve.public(&b).unwrap());
            assert_eq!(curve.ecdh(&a, &pb).unwrap(), curve.ecdh(&b, &pa).unwrap(), "{}", curve.name);
            let compressed = curve.convert(&pa, 2).unwrap();
            assert_eq!(curve.convert(&curve.convert(&compressed, 6).unwrap(), 4).unwrap(), pa);
            let digest = [7u8; 32];
            let signature = curve.sign(&a, &digest).unwrap();
            assert!(curve.verify(&pa, &digest, &signature), "{}", curve.name);
            assert!(!curve.verify(&pb, &digest, &signature), "{}", curve.name);
        }
    }

    #[test]
    fn matches_rfc6979_vectors() {
        let curve = curve("P-256").unwrap();
        let d = unhex("c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721");
        let point = curve.public(&d).unwrap();
        assert_eq!(hex(&point[1..33]), "60fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6");
        assert_eq!(hex(&point[33..]), "7903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299");
        let digest = ring::digest::digest(&ring::digest::SHA256, b"sample");
        let signature = unhex(concat!(
            "efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716",
            "f7cb1c942d657c41d436c7a1b6e29f65f3e900dbb9aff4064dc4ab2f843acda8"
        ));
        assert!(curve.verify(&point, digest.as_ref(), &signature));
        assert_eq!(curve.sign(&d, digest.as_ref()).unwrap(), signature);
    }
}
