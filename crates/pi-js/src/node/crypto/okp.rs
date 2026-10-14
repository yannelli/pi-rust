//! Octet-string keys: Ed25519 (ring), X25519 (x25519-dalek), Ed448 and X448 (ed448-goldilocks, x448).

use crate::error::JsError;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Okp {
    Ed25519,
    X25519,
    Ed448,
    X448,
}

const ALL: [Okp; 4] = [Okp::Ed25519, Okp::X25519, Okp::Ed448, Okp::X448];

fn bad_key() -> JsError {
    JsError::new("error:1E08010C:DECODER routines::unsupported").with_code("ERR_OSSL_UNSUPPORTED")
}

fn agreement_failed(name: &str) -> JsError {
    JsError::new(format!("Failed to perform {name} key agreement")).with_code("ERR_CRYPTO_OPERATION_FAILED")
}

impl Okp {
    pub(super) fn from_oid(oid: &str) -> Option<Self> {
        ALL.into_iter().find(|kind| kind.oid() == oid)
    }

    /// The kind for a JWK `crv` or a Node key type name.
    pub(super) fn from_name(name: &str) -> Option<Self> {
        ALL.into_iter().find(|kind| kind.jwk() == name || kind.name() == name)
    }

    pub(super) fn oid(self) -> &'static str {
        match self {
            Self::X25519 => "1.3.101.110",
            Self::X448 => "1.3.101.111",
            Self::Ed25519 => "1.3.101.112",
            Self::Ed448 => "1.3.101.113",
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Ed25519 => "ed25519",
            Self::X25519 => "x25519",
            Self::Ed448 => "ed448",
            Self::X448 => "x448",
        }
    }

    pub(super) fn jwk(self) -> &'static str {
        match self {
            Self::Ed25519 => "Ed25519",
            Self::X25519 => "X25519",
            Self::Ed448 => "Ed448",
            Self::X448 => "X448",
        }
    }

    /// Byte length of the private seed or scalar, which is also the public key length.
    pub(super) fn size(self) -> usize {
        match self {
            Self::Ed25519 | Self::X25519 => 32,
            Self::Ed448 => 57,
            Self::X448 => 56,
        }
    }

    pub(super) fn is_signing(self) -> bool {
        matches!(self, Self::Ed25519 | Self::Ed448)
    }

    pub(super) fn public(self, private: &[u8]) -> Result<Vec<u8>, JsError> {
        if private.len() != self.size() {
            return Err(bad_key());
        }
        match self {
            Self::Ed25519 => {
                let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(private).map_err(|_| bad_key())?;
                Ok(ring::signature::KeyPair::public_key(&pair).as_ref().to_vec())
            }
            Self::X25519 => {
                let secret = x25519_dalek::StaticSecret::from(<[u8; 32]>::try_from(private).map_err(|_| bad_key())?);
                Ok(x25519_dalek::PublicKey::from(&secret).as_bytes().to_vec())
            }
            Self::Ed448 => {
                let key = ed448_goldilocks::SigningKey::try_from(private).map_err(|_| bad_key())?;
                Ok(key.verifying_key().as_bytes().to_vec())
            }
            Self::X448 => {
                let base = ed448_goldilocks::MontgomeryPoint::GENERATOR.0;
                let scalar = <[u8; 56]>::try_from(private).map_err(|_| bad_key())?;
                x448::x448(scalar, base).map(|point| point.to_vec()).ok_or_else(bad_key)
            }
        }
    }

    pub(super) fn sign(self, private: &[u8], data: &[u8]) -> Result<Vec<u8>, JsError> {
        match self {
            Self::Ed25519 => {
                let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(private).map_err(|_| bad_key())?;
                Ok(pair.sign(data).as_ref().to_vec())
            }
            Self::Ed448 => {
                let key = ed448_goldilocks::SigningKey::try_from(private).map_err(|_| bad_key())?;
                Ok(key.sign_raw(data).to_bytes().to_vec())
            }
            _ => Err(super::ops::not_supported_for_key()),
        }
    }

    pub(super) fn verify(self, public: &[u8], data: &[u8], signature: &[u8]) -> Result<bool, JsError> {
        match self {
            Self::Ed25519 => {
                let key = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public);
                Ok(key.verify(data, signature).is_ok())
            }
            Self::Ed448 => {
                let Ok(bytes) = public.try_into() else { return Ok(false) };
                let Ok(key) = ed448_goldilocks::VerifyingKey::from_bytes(&bytes) else { return Ok(false) };
                let Ok(signature) = ed448_goldilocks::Signature::from_slice(signature) else { return Ok(false) };
                Ok(key.verify_raw(&signature, data).is_ok())
            }
            _ => Err(super::ops::not_supported_for_key()),
        }
    }

    /// X25519 or X448 agreement; an all-zero result (a low-order peer point) fails as in OpenSSL.
    pub(super) fn agree(self, private: &[u8], public: &[u8]) -> Result<Vec<u8>, JsError> {
        match self {
            Self::X25519 => {
                let secret = x25519_dalek::StaticSecret::from(<[u8; 32]>::try_from(private).map_err(|_| bad_key())?);
                let peer = x25519_dalek::PublicKey::from(<[u8; 32]>::try_from(public).map_err(|_| bad_key())?);
                let shared = secret.diffie_hellman(&peer);
                if !shared.was_contributory() {
                    return Err(agreement_failed("X25519"));
                }
                Ok(shared.as_bytes().to_vec())
            }
            Self::X448 => {
                let scalar = <[u8; 56]>::try_from(private).map_err(|_| bad_key())?;
                let peer = <[u8; 56]>::try_from(public).map_err(|_| bad_key())?;
                x448::x448(scalar, peer).map(|point| point.to_vec()).ok_or_else(|| agreement_failed("X448"))
            }
            _ => Err(super::ops::not_supported_for_key()),
        }
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
    fn matches_rfc7748_x25519_and_x448() {
        let scalar = unhex("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4");
        let u = unhex("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c");
        assert_eq!(
            hex(&Okp::X25519.agree(&scalar, &u).unwrap()),
            "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552"
        );
        let alice = unhex(concat!(
            "9a8f4925d1519f5775cf46b04b5800d4ee9ee8bae8bc5565d498c28d",
            "d9c9baf574a9419744897391006382a6f127ab1d9ac2d8c0a598726b"
        ));
        let bob_public = unhex(concat!(
            "3eb7a829b0cd20f5bcfc0b599b6feccf6da4627107bdb0d4f345b430",
            "27d8b972fc3e34fb4232a13ca706dcb57aec3dae07bdc1c67bf33609"
        ));
        assert_eq!(
            hex(&Okp::X448.public(&alice).unwrap()),
            concat!(
                "9b08f7cc31b7e3e67d22d5aea121074a273bd2b83de09c63faa73d2c",
                "22c5d9bbc836647241d953d40c5b12da88120d53177f80e532c41fa0"
            )
        );
        assert_eq!(
            hex(&Okp::X448.agree(&alice, &bob_public).unwrap()),
            concat!(
                "07fff4181ac6cc95ec1c16a94a0f74d12da232ce40a77552281d282b",
                "b60c0b56fd2464c335543936521c24403085d59a449a5037514a879d"
            )
        );
    }

    #[test]
    fn matches_rfc8032_ed448() {
        let seed = unhex(concat!(
            "6c82a562cb808d10d632be89c8513ebf6c929f34ddfa8c9f63c9960ef6e348a3",
            "528c8a3fcc2f044e39a3fc5b94492f8f032e7549a20098f95b"
        ));
        let public = Okp::Ed448.public(&seed).unwrap();
        assert_eq!(
            hex(&public),
            concat!(
                "5fd7449b59b461fd2ce787ec616ad46a1da1342485a70e1f8a0ea75d80e96778",
                "edf124769b46c7061bd6783df1e50f6cd1fa1abeafe8256180"
            )
        );
        let signature = Okp::Ed448.sign(&seed, &[]).unwrap();
        assert_eq!(
            hex(&signature),
            concat!(
                "533a37f6bbe457251f023c0d88f976ae2dfb504a843e34d2074fd823d41a591f",
                "2b233f034f628281f2fd7a22ddd47d7828c59bd0a21bfd3980ff0d2028d4b18a",
                "9df63e006c5d1c2d345b925d8dc00b4104852db99ac5c7cdda8530a113a0f4db",
                "b61149f05a7363268c71d95808ff2e652600"
            )
        );
        assert!(Okp::Ed448.verify(&public, &[], &signature).unwrap());
        assert!(!Okp::Ed448.verify(&public, &[1], &signature).unwrap());
    }
}
