//! RSA over the RustCrypto `rsa` crate (generation, encryption, decryption, PSS and PKCS#1 v1.5
//! signatures), with ring signing the PKCS#1 v1.5 and digest-length PSS cases it supports.
//! Private-key operations in `rsa` run with blinding (RUSTSEC-2023-0071 has no fixed release).

use rsa::hazmat::{rsa_decrypt_and_check, rsa_encrypt};
use rsa::rand_core::OsRng;
use rsa::sha2::digest::DynDigest;
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{BigUint, Oaep, Pkcs1v15Encrypt, Pkcs1v15Sign, Pss, RsaPrivateKey, RsaPublicKey};

use super::der::{self, fixed};
use super::digest;
use crate::error::JsError;

pub(super) const PKCS1_PADDING: i32 = 1;
pub(super) const NO_PADDING: i32 = 3;
pub(super) const OAEP_PADDING: i32 = 4;
pub(super) const PSS_PADDING: i32 = 6;
pub(super) const SALTLEN_DIGEST: i32 = -1;
pub(super) const SALTLEN_MAX: i32 = -2;

/// The largest modulus Node accepts.
const MAX_BITS: usize = 16384;

#[derive(Clone)]
pub(super) struct Rsa {
    pub public: RsaPublicKey,
    pub private: Option<Box<RsaPrivateKey>>,
}

fn openssl(code: &str, reason: &str, id: &str) -> JsError {
    JsError::new(format!("error:{id}:rsa routines::{reason}")).with_code(format!("ERR_OSSL_RSA_{code}"))
}

fn too_large() -> JsError {
    openssl("DATA_TOO_LARGE_FOR_KEY_SIZE", "data too large for key size", "0200006E")
}

fn bad_key() -> JsError {
    JsError::new("error:1E08010C:DECODER routines::unsupported").with_code("ERR_OSSL_UNSUPPORTED")
}

fn unknown_padding() -> JsError {
    openssl("UNKNOWN_PADDING_TYPE", "unknown padding type", "02000076")
}

fn invalid_digest(hash: &str) -> JsError {
    JsError::type_error(format!("Invalid digest: {hash}")).with_code("ERR_CRYPTO_INVALID_DIGEST")
}

fn digest_oid(hash: &str) -> Result<&'static str, JsError> {
    Ok(match hash {
        "md5" => "1.2.840.113549.2.5",
        "sha1" => "1.3.14.3.2.26",
        "sha256" => "2.16.840.1.101.3.4.2.1",
        "sha384" => "2.16.840.1.101.3.4.2.2",
        "sha512" => "2.16.840.1.101.3.4.2.3",
        "sha512-256" => "2.16.840.1.101.3.4.2.6",
        _ => return Err(invalid_digest(hash)),
    })
}

fn dyn_digest(hash: &str) -> Result<Box<dyn DynDigest + Send + Sync>, JsError> {
    use rsa::sha2::Digest;
    Ok(match hash {
        "md5" => Box::new(md5::Md5::new()),
        "sha1" => Box::new(sha1::Sha1::new()),
        "sha256" => Box::new(rsa::sha2::Sha256::new()),
        "sha384" => Box::new(rsa::sha2::Sha384::new()),
        "sha512" => Box::new(rsa::sha2::Sha512::new()),
        "sha512-256" => Box::new(rsa::sha2::Sha512_256::new()),
        _ => return Err(invalid_digest(hash)),
    })
}

/// PKCS#1 v1.5 signature padding with the DigestInfo prefix for `hash`.
fn pkcs1_scheme(hash: &str, hash_len: usize) -> Result<Pkcs1v15Sign, JsError> {
    let info = der::seq(&[der::seq(&[der::oid(digest_oid(hash)?), der::null()]), der::octets(&vec![0; hash_len])]);
    Ok(Pkcs1v15Sign { hash_len: Some(hash_len), prefix: info[..info.len() - hash_len].into() })
}

fn pss_scheme(hash: &str, salt_len: usize) -> Result<Pss, JsError> {
    Ok(Pss { blinded: false, digest: dyn_digest(hash)?, salt_len })
}

fn oaep_scheme(hash: &str, label: &[u8]) -> Result<Oaep, JsError> {
    let label = match std::str::from_utf8(label) {
        Ok("") => None,
        Ok(text) => Some(text.to_string()),
        Err(_) => return Err(JsError::unsupported("crypto RSA-OAEP labels that are not UTF-8")),
    };
    Ok(Oaep { digest: dyn_digest(hash)?, mgf_digest: dyn_digest(hash)?, label })
}

fn ring_algorithm(hash: &str, padding: i32) -> Option<&'static dyn ring::signature::RsaEncoding> {
    use ring::signature::*;
    Some(match (hash, padding) {
        ("sha256", PKCS1_PADDING) => &RSA_PKCS1_SHA256,
        ("sha384", PKCS1_PADDING) => &RSA_PKCS1_SHA384,
        ("sha512", PKCS1_PADDING) => &RSA_PKCS1_SHA512,
        ("sha256", PSS_PADDING) => &RSA_PSS_SHA256,
        ("sha384", PSS_PADDING) => &RSA_PSS_SHA384,
        ("sha512", PSS_PADDING) => &RSA_PSS_SHA512,
        _ => return None,
    })
}

fn big(bytes: &[u8]) -> BigUint {
    BigUint::from_bytes_be(bytes)
}

impl Rsa {
    pub(super) fn from_public(n: &[u8], e: &[u8]) -> Result<Self, JsError> {
        let public = RsaPublicKey::new_with_max_size(big(n), big(e), MAX_BITS).map_err(|_| bad_key())?;
        Ok(Self { public, private: None })
    }

    /// A private key from `n`, `e`, `d`, `p`, and `q`; the CRT values are recomputed.
    pub(super) fn from_private(n: &[u8], e: &[u8], d: &[u8], p: &[u8], q: &[u8]) -> Result<Self, JsError> {
        let private =
            RsaPrivateKey::from_components(big(n), big(e), big(d), vec![big(p), big(q)]).map_err(|_| bad_key())?;
        Ok(Self { public: private.to_public_key(), private: Some(Box::new(private)) })
    }

    pub(super) fn generate(bits: u64, e: u64) -> Result<Self, JsError> {
        if bits < 512 {
            return Err(JsError::new("error:1C8000B3:Provider routines::key size too small")
                .with_code("ERR_OSSL_KEY_SIZE_TOO_SMALL"));
        }
        if e < 3 || e % 2 == 0 {
            return Err(JsError::new("error:1C80006B:Provider routines::invalid public exponent")
                .with_code("ERR_OSSL_BAD_E_VALUE"));
        }
        let private = RsaPrivateKey::new_with_exp(&mut OsRng, bits as usize, &BigUint::from(e))
            .map_err(|error| JsError::new(format!("RSA key generation failed: {error}")))?;
        Ok(Self { public: private.to_public_key(), private: Some(Box::new(private)) })
    }

    pub(super) fn n(&self) -> Vec<u8> {
        self.public.n().to_bytes_be()
    }

    pub(super) fn e(&self) -> Vec<u8> {
        self.public.e().to_bytes_be()
    }

    pub(super) fn bits(&self) -> usize {
        self.public.n().bits()
    }

    pub(super) fn exponent(&self) -> String {
        self.public.e().to_string()
    }

    fn size(&self) -> usize {
        self.public.size()
    }

    /// `n, e, d, p, q, dp, dq, qinv` of a two-prime private key.
    pub(super) fn private_parts(&self) -> Option<[Vec<u8>; 8]> {
        let key = self.private.as_deref()?;
        let [p, q] = key.primes() else { return None };
        let bytes = |value: &BigUint| value.to_bytes_be();
        Some([
            self.n(),
            self.e(),
            bytes(key.d()),
            bytes(p),
            bytes(q),
            bytes(key.dp()?),
            bytes(key.dq()?),
            bytes(&key.crt_coefficient()?),
        ])
    }

    fn private_key(&self) -> Result<&RsaPrivateKey, JsError> {
        self.private.as_deref().ok_or_else(|| JsError::type_error("A private key is required"))
    }

    fn max_salt(&self, hash_len: usize) -> Result<usize, JsError> {
        (self.bits() - 1)
            .div_ceil(8)
            .checked_sub(hash_len + 2)
            .ok_or_else(|| openssl("KEY_SIZE_TOO_SMALL", "key size too small", "02000078"))
    }

    pub(super) fn sign(&self, hash: &str, data: &[u8], padding: i32, salt_length: i32) -> Result<Vec<u8>, JsError> {
        let key = self.private_key()?;
        let hashed = digest(hash, data)?;
        let ring_salt_ok = padding == PKCS1_PADDING || salt_length == SALTLEN_DIGEST;
        if let (true, Some(algorithm), Some(parts)) =
            (ring_salt_ok, ring_algorithm(hash, padding), self.private_parts())
            && let Ok(pair) = ring::signature::RsaKeyPair::from_der(&pkcs1_private_der(&parts))
        {
            let mut signature = vec![0; pair.public().modulus_len()];
            let rng = ring::rand::SystemRandom::new();
            if pair.sign(algorithm, &rng, data, &mut signature).is_ok() {
                return Ok(signature);
            }
        }
        let result = if padding == PSS_PADDING {
            let salt = match salt_length {
                SALTLEN_DIGEST => hashed.len(),
                length if length < 0 => self.max_salt(hashed.len())?,
                length => length as usize,
            };
            if salt > self.max_salt(hashed.len())? {
                return Err(too_large());
            }
            key.sign_with_rng(&mut OsRng, pss_scheme(hash, salt)?, &hashed)
        } else {
            key.sign_with_rng(&mut OsRng, pkcs1_scheme(hash, hashed.len())?, &hashed)
        };
        result.map_err(|_| openssl("DIGEST_TOO_BIG_FOR_RSA_KEY", "digest too big for rsa key", "02000070"))
    }

    /// Check a signature; a negative PSS `salt_length` other than `SALTLEN_DIGEST` takes any salt length.
    pub(super) fn verify(
        &self,
        hash: &str,
        data: &[u8],
        signature: &[u8],
        padding: i32,
        salt_length: i32,
    ) -> Result<bool, JsError> {
        let hashed = digest(hash, data)?;
        if signature.len() != self.size() {
            return Ok(false);
        }
        if padding != PSS_PADDING {
            return Ok(self.public.verify(pkcs1_scheme(hash, hashed.len())?, &hashed, signature).is_ok());
        }
        let check = |salt: usize| -> Result<bool, JsError> {
            Ok(self.public.verify(pss_scheme(hash, salt)?, &hashed, signature).is_ok())
        };
        match salt_length {
            SALTLEN_DIGEST => check(hashed.len()),
            length if length >= 0 => check(length as usize),
            _ => {
                let Ok(max) = self.max_salt(hashed.len()) else { return Ok(false) };
                let common = [hashed.len(), max];
                for salt in common.into_iter().chain((0..=max).filter(|salt| !common.contains(salt))) {
                    if salt <= max && check(salt)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    fn raw_public(&self, data: &[u8]) -> Result<Vec<u8>, JsError> {
        let value = rsa_encrypt(&self.public, &big(data))
            .map_err(|_| openssl("DATA_TOO_LARGE_FOR_MODULUS", "data too large for modulus", "02000084"))?;
        Ok(fixed(&value.to_bytes_be(), self.size()))
    }

    fn raw_private(&self, data: &[u8]) -> Result<Vec<u8>, JsError> {
        let value = rsa_decrypt_and_check(self.private_key()?, Some(&mut OsRng), &big(data))
            .map_err(|_| openssl("DATA_TOO_LARGE_FOR_MODULUS", "data too large for modulus", "02000084"))?;
        Ok(fixed(&value.to_bytes_be(), self.size()))
    }

    /// `publicEncrypt` (`private == false`) or `privateEncrypt`.
    pub(super) fn encrypt(
        &self,
        private: bool,
        data: &[u8],
        padding: i32,
        oaep_hash: &str,
        label: &[u8],
    ) -> Result<Vec<u8>, JsError> {
        match padding {
            NO_PADDING if data.len() != self.size() => Err(too_large()),
            NO_PADDING if private => self.raw_private(data),
            NO_PADDING => self.raw_public(data),
            OAEP_PADDING if !private => {
                self.public.encrypt(&mut OsRng, oaep_scheme(oaep_hash, label)?, data).map_err(|_| too_large())
            }
            PKCS1_PADDING if private => self
                .private_key()?
                .sign_with_rng(&mut OsRng, Pkcs1v15Sign::new_unprefixed(), data)
                .map_err(|_| too_large()),
            PKCS1_PADDING => self.public.encrypt(&mut OsRng, Pkcs1v15Encrypt, data).map_err(|_| too_large()),
            _ => Err(unknown_padding()),
        }
    }

    /// `privateDecrypt` (`private == true`) or `publicDecrypt`.
    pub(super) fn decrypt(
        &self,
        private: bool,
        data: &[u8],
        padding: i32,
        oaep_hash: &str,
        label: &[u8],
    ) -> Result<Vec<u8>, JsError> {
        if data.len() > self.size() {
            return Err(openssl("DATA_GREATER_THAN_MOD_LEN", "data greater than mod len", "0200006C"));
        }
        match padding {
            NO_PADDING if private => self.raw_private(data),
            NO_PADDING => self.raw_public(data),
            OAEP_PADDING if private => self
                .private_key()?
                .decrypt_blinded(&mut OsRng, oaep_scheme(oaep_hash, label)?, data)
                .map_err(|_| openssl("OAEP_DECODING_ERROR", "oaep decoding error", "02000079")),
            PKCS1_PADDING if private => self
                .private_key()?
                .decrypt_blinded(&mut OsRng, Pkcs1v15Encrypt, data)
                .map_err(|_| openssl("PKCS_DECODING_ERROR", "pkcs decoding error", "0200009F")),
            PKCS1_PADDING => {
                // Block type 1 carries public data, so a plain scan is fine here.
                let em = self.raw_public(data)?;
                let end = em.iter().skip(2).position(|byte| *byte == 0).map(|index| index + 2);
                match end {
                    Some(end) if em[..2] == [0, 1] && end >= 10 && em[2..end].iter().all(|b| *b == 0xff) => {
                        Ok(em[end + 1..].to_vec())
                    }
                    _ => Err(openssl("PADDING_CHECK_FAILED", "padding check failed", "02000072")),
                }
            }
            _ => Err(unknown_padding()),
        }
    }
}

/// The PKCS#1 `RSAPrivateKey` DER for `n, e, d, p, q, dp, dq, qinv`.
pub(super) fn pkcs1_private_der(parts: &[Vec<u8>; 8]) -> Vec<u8> {
    let mut fields = vec![der::uint(&[0])];
    fields.extend(parts.iter().map(|part| der::uint(part)));
    der::seq(&fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_and_encrypts_with_a_generated_key() {
        let key = Rsa::generate(512, 65537).unwrap();
        assert_eq!(key.bits(), 512);
        for (hash, padding, salt) in
            [("sha256", PKCS1_PADDING, SALTLEN_MAX), ("sha1", PSS_PADDING, SALTLEN_MAX), ("sha256", PSS_PADDING, 5)]
        {
            let signature = key.sign(hash, b"hello", padding, salt).unwrap();
            assert!(key.verify(hash, b"hello", &signature, padding, SALTLEN_MAX).unwrap());
            assert!(!key.verify(hash, b"hellO", &signature, padding, SALTLEN_MAX).unwrap());
        }
        for padding in [PKCS1_PADDING, OAEP_PADDING] {
            let sealed = key.encrypt(false, b"secret", padding, "sha1", b"").unwrap();
            assert_eq!(key.decrypt(true, &sealed, padding, "sha1", b"").unwrap(), b"secret");
        }
        let sealed = key.encrypt(true, b"secret", PKCS1_PADDING, "sha1", b"").unwrap();
        assert_eq!(key.decrypt(false, &sealed, PKCS1_PADDING, "sha1", b"").unwrap(), b"secret");
    }

    #[test]
    fn ring_and_rsa_signatures_interoperate() {
        let key = Rsa::generate(2048, 65537).unwrap();
        let pkcs1 = key.sign("sha256", b"hello", PKCS1_PADDING, SALTLEN_MAX).unwrap();
        assert!(key.verify("sha256", b"hello", &pkcs1, PKCS1_PADDING, SALTLEN_MAX).unwrap());
        let pss = key.sign("sha256", b"hello", PSS_PADDING, SALTLEN_DIGEST).unwrap();
        assert!(key.verify("sha256", b"hello", &pss, PSS_PADDING, 32).unwrap());
        let public = ring::signature::RsaPublicKeyComponents { n: key.n(), e: key.e() };
        let max = key.sign("sha256", b"hello", PSS_PADDING, SALTLEN_MAX).unwrap();
        assert!(public.verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, b"hello", &pkcs1).is_ok());
        assert!(key.verify("sha256", b"hello", &max, PSS_PADDING, SALTLEN_MAX).unwrap());
    }
}
