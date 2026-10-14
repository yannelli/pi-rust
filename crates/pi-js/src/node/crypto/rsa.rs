//! RSA over num-bigint: key generation, PKCS#1 v1.5 and PSS signatures, PKCS#1 v1.5 and OAEP encryption.

use num_bigint::BigUint;
use num_traits::{One, Zero};

use super::bigint::{generate_prime, to_fixed};
use super::{der, digest, random_bytes};
use crate::error::JsError;

pub(super) const PKCS1_PADDING: i32 = 1;
pub(super) const NO_PADDING: i32 = 3;
pub(super) const OAEP_PADDING: i32 = 4;
pub(super) const PSS_PADDING: i32 = 6;
pub(super) const SALTLEN_DIGEST: i32 = -1;
pub(super) const SALTLEN_MAX: i32 = -2;

#[derive(Clone)]
pub(super) struct RsaSecret {
    pub d: BigUint,
    pub p: BigUint,
    pub q: BigUint,
    pub dp: BigUint,
    pub dq: BigUint,
    pub qinv: BigUint,
}

#[derive(Clone)]
pub(super) struct Rsa {
    pub n: BigUint,
    pub e: BigUint,
    pub secret: Option<RsaSecret>,
}

fn openssl(code: &str, reason: &str, id: &str) -> JsError {
    JsError::new(format!("error:{id}:rsa routines::{reason}")).with_code(format!("ERR_OSSL_RSA_{code}"))
}

fn too_large() -> JsError {
    openssl("DATA_TOO_LARGE_FOR_KEY_SIZE", "data too large for key size", "0200006E")
}

fn digest_oid(hash: &str) -> Result<&'static str, JsError> {
    Ok(match hash {
        "md5" => "1.2.840.113549.2.5",
        "sha1" => "1.3.14.3.2.26",
        "sha256" => "2.16.840.1.101.3.4.2.1",
        "sha384" => "2.16.840.1.101.3.4.2.2",
        "sha512" => "2.16.840.1.101.3.4.2.3",
        "sha512-256" => "2.16.840.1.101.3.4.2.6",
        _ => return Err(JsError::type_error(format!("Invalid digest: {hash}")).with_code("ERR_CRYPTO_INVALID_DIGEST")),
    })
}

fn mgf1(hash: &str, seed: &[u8], length: usize) -> Result<Vec<u8>, JsError> {
    let mut out = Vec::with_capacity(length);
    let mut counter = 0u32;
    while out.len() < length {
        out.extend(digest(hash, &[seed, &counter.to_be_bytes()].concat())?);
        counter += 1;
    }
    out.truncate(length);
    Ok(out)
}

fn xor(target: &mut [u8], mask: &[u8]) {
    target.iter_mut().zip(mask).for_each(|(byte, mask)| *byte ^= mask);
}

impl Rsa {
    pub(super) fn generate(bits: u64, e: &BigUint) -> Result<Self, JsError> {
        if bits < 512 {
            return Err(JsError::new("error:1C8000B3:Provider routines::key size too small")
                .with_code("ERR_OSSL_KEY_SIZE_TOO_SMALL"));
        }
        if e < &BigUint::from(3u8) || !e.bit(0) {
            return Err(JsError::new("error:1C80006B:Provider routines::invalid public exponent")
                .with_code("ERR_OSSL_BAD_E_VALUE"));
        }
        let one = BigUint::one();
        loop {
            let p = generate_prime(bits.div_ceil(2), false, None, None)?;
            let q = generate_prime(bits / 2, false, None, None)?;
            let (p, q) = if p > q { (p, q) } else { (q, p) };
            let phi = (&p - &one) * (&q - &one);
            let n = &p * &q;
            let (Some(d), Some(qinv)) = (e.modinv(&phi), q.modinv(&p)) else { continue };
            if p == q || n.bits() != bits {
                continue;
            }
            let secret = RsaSecret { dp: &d % (&p - &one), dq: &d % (&q - &one), d, p, q, qinv };
            return Ok(Self { n, e: e.clone(), secret: Some(secret) });
        }
    }

    pub(super) fn size(&self) -> usize {
        self.n.bits().div_ceil(8) as usize
    }

    fn public_op(&self, input: &[u8]) -> Result<Vec<u8>, JsError> {
        let m = BigUint::from_bytes_be(input);
        if m >= self.n {
            return Err(openssl("DATA_TOO_LARGE_FOR_MODULUS", "data too large for modulus", "02000084"));
        }
        Ok(to_fixed(&m.modpow(&self.e, &self.n), self.size()))
    }

    fn private_op(&self, input: &[u8]) -> Result<Vec<u8>, JsError> {
        let secret = self.secret.as_ref().ok_or_else(|| JsError::type_error("A private key is required"))?;
        let c = BigUint::from_bytes_be(input);
        if c >= self.n {
            return Err(openssl("DATA_TOO_LARGE_FOR_MODULUS", "data too large for modulus", "02000084"));
        }
        let m = if secret.p.is_zero() {
            c.modpow(&secret.d, &self.n)
        } else {
            let m1 = c.modpow(&secret.dp, &secret.p);
            let m2 = c.modpow(&secret.dq, &secret.q);
            let h = (&secret.qinv * (&m1 + &secret.p - (&m2 % &secret.p))) % &secret.p;
            m2 + h * &secret.q
        };
        Ok(to_fixed(&m, self.size()))
    }

    pub(super) fn sign(&self, hash: &str, data: &[u8], padding: i32, salt_length: i32) -> Result<Vec<u8>, JsError> {
        let hashed = digest(hash, data)?;
        let encoded = if padding == PSS_PADDING {
            self.pss_encode(hash, &hashed, salt_length)?
        } else {
            self.pkcs1_digest_info(hash, &hashed)?
        };
        self.private_op(&encoded)
    }

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
        let Ok(decoded) = self.public_op(signature) else { return Ok(false) };
        if padding == PSS_PADDING {
            return self.pss_verify(hash, &hashed, &decoded, salt_length);
        }
        Ok(self.pkcs1_digest_info(hash, &hashed).is_ok_and(|expected| expected == decoded))
    }

    fn pkcs1_digest_info(&self, hash: &str, hashed: &[u8]) -> Result<Vec<u8>, JsError> {
        let info = der::seq(&[der::seq(&[der::oid(digest_oid(hash)?), der::null()]), der::octets(hashed)]);
        let size = self.size();
        if size < info.len() + 11 {
            return Err(openssl("DIGEST_TOO_BIG_FOR_RSA_KEY", "digest too big for rsa key", "02000070"));
        }
        Ok([vec![0, 1], vec![0xff; size - info.len() - 3], vec![0], info].concat())
    }

    fn pss_encode(&self, hash: &str, hashed: &[u8], salt_length: i32) -> Result<Vec<u8>, JsError> {
        let em_bits = self.n.bits() as usize - 1;
        let em_len = em_bits.div_ceil(8);
        let h_len = hashed.len();
        let max = em_len
            .checked_sub(h_len + 2)
            .ok_or_else(|| openssl("KEY_SIZE_TOO_SMALL", "key size too small", "02000078"))?;
        let salt_len = match salt_length {
            SALTLEN_DIGEST => h_len,
            length if length < 0 => max,
            length => length as usize,
        };
        if salt_len > max {
            return Err(too_large());
        }
        let salt = random_bytes(salt_len)?;
        let h = digest(hash, &[&[0u8; 8], hashed, &salt].concat())?;
        let mut db = [vec![0; em_len - salt_len - h_len - 2], vec![1], salt].concat();
        let mask = mgf1(hash, &h, db.len())?;
        xor(&mut db, &mask);
        db[0] &= 0xff >> (8 * em_len - em_bits);
        Ok([db, h, vec![0xbc]].concat())
    }

    fn pss_verify(&self, hash: &str, hashed: &[u8], decoded: &[u8], salt_length: i32) -> Result<bool, JsError> {
        let em_bits = self.n.bits() as usize - 1;
        let em_len = em_bits.div_ceil(8);
        let h_len = hashed.len();
        let (padding, em) = decoded.split_at(decoded.len() - em_len);
        if padding.iter().any(|byte| *byte != 0) || em_len < h_len + 2 || em[em_len - 1] != 0xbc {
            return Ok(false);
        }
        let (masked, h) = em[..em_len - 1].split_at(em_len - h_len - 1);
        let top_mask = 0xffu8 >> (8 * em_len - em_bits);
        if masked[0] & !top_mask != 0 {
            return Ok(false);
        }
        let mut db = masked.to_vec();
        let mask = mgf1(hash, h, db.len())?;
        xor(&mut db, &mask);
        db[0] &= top_mask;
        let Some(start) = db.iter().position(|byte| *byte != 0) else { return Ok(false) };
        let salt = &db[start + 1..];
        let expected_len = match salt_length {
            SALTLEN_DIGEST => Some(h_len),
            length if length < 0 => None,
            length => Some(length as usize),
        };
        if db[start] != 1 || expected_len.is_some_and(|length| length != salt.len()) {
            return Ok(false);
        }
        Ok(digest(hash, &[&[0u8; 8], hashed, salt].concat())? == h)
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
        let size = self.size();
        let encoded = match padding {
            NO_PADDING if data.len() == size => data.to_vec(),
            NO_PADDING => {
                return Err(openssl("DATA_TOO_LARGE_FOR_KEY_SIZE", "data too large for key size", "0200006E"));
            }
            OAEP_PADDING if !private => {
                let l_hash = digest(oaep_hash, label)?;
                let h_len = l_hash.len();
                if data.len() + 2 * h_len + 2 > size {
                    return Err(too_large());
                }
                let mut db = [l_hash, vec![0; size - data.len() - 2 * h_len - 2], vec![1], data.to_vec()].concat();
                let mut seed = random_bytes(h_len)?;
                let mask = mgf1(oaep_hash, &seed, db.len())?;
                xor(&mut db, &mask);
                xor(&mut seed, &mgf1(oaep_hash, &db, h_len)?);
                [vec![0], seed, db].concat()
            }
            PKCS1_PADDING => {
                if data.len() + 11 > size {
                    return Err(too_large());
                }
                let mut fill = vec![0xff; size - data.len() - 3];
                if !private {
                    for byte in &mut fill {
                        while *byte == 0 || *byte == 0xff {
                            *byte = random_bytes(1)?[0];
                        }
                    }
                }
                [vec![0, if private { 1 } else { 2 }], fill, vec![0], data.to_vec()].concat()
            }
            _ => return Err(openssl("UNKNOWN_PADDING_TYPE", "unknown padding type", "02000076")),
        };
        if private { self.private_op(&encoded) } else { self.public_op(&encoded) }
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
        let em = if private { self.private_op(data)? } else { self.public_op(data)? };
        match padding {
            NO_PADDING => Ok(em),
            OAEP_PADDING if private => {
                let failure = || openssl("OAEP_DECODING_ERROR", "oaep decoding error", "02000079");
                let l_hash = digest(oaep_hash, label)?;
                let h_len = l_hash.len();
                if em.len() < 2 * h_len + 2 {
                    return Err(failure());
                }
                let (seed, db) = em[1..].split_at(h_len);
                let mut seed = seed.to_vec();
                xor(&mut seed, &mgf1(oaep_hash, db, h_len)?);
                let mut db = db.to_vec();
                let mask = mgf1(oaep_hash, &seed, db.len())?;
                xor(&mut db, &mask);
                let rest = &db[h_len..];
                match rest.iter().position(|byte| *byte != 0) {
                    Some(index) if em[0] == 0 && db[..h_len] == l_hash[..] && rest[index] == 1 => {
                        Ok(rest[index + 1..].to_vec())
                    }
                    _ => Err(failure()),
                }
            }
            PKCS1_PADDING => {
                let block = if private { 2 } else { 1 };
                let end = em.iter().skip(2).position(|byte| *byte == 0).map(|index| index + 2);
                match end {
                    Some(end)
                        if em[0] == 0
                            && em[1] == block
                            && end >= 10
                            && (private || em[2..end].iter().all(|b| *b == 0xff)) =>
                    {
                        Ok(em[end + 1..].to_vec())
                    }
                    _ if private => Err(openssl("PKCS_DECODING_ERROR", "pkcs decoding error", "0200009F")),
                    _ => Err(openssl("PADDING_CHECK_FAILED", "padding check failed", "02000072")),
                }
            }
            _ => Err(openssl("UNKNOWN_PADDING_TYPE", "unknown padding type", "02000076")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_and_encrypts_with_a_generated_key() {
        let key = Rsa::generate(512, &BigUint::from(65537u32)).unwrap();
        assert_eq!(key.n.bits(), 512);
        for padding in [PKCS1_PADDING, PSS_PADDING] {
            let signature = key.sign("sha256", b"hello", padding, SALTLEN_MAX).unwrap();
            assert!(key.verify("sha256", b"hello", &signature, padding, SALTLEN_MAX).unwrap());
            assert!(!key.verify("sha256", b"hellO", &signature, padding, SALTLEN_MAX).unwrap());
        }
        for padding in [PKCS1_PADDING, OAEP_PADDING] {
            let sealed = key.encrypt(false, b"secret", padding, "sha1", b"").unwrap();
            assert_eq!(key.decrypt(true, &sealed, padding, "sha1", b"").unwrap(), b"secret");
        }
        let sealed = key.encrypt(true, b"secret", PKCS1_PADDING, "sha1", b"").unwrap();
        assert_eq!(key.decrypt(false, &sealed, PKCS1_PADDING, "sha1", b"").unwrap(), b"secret");
    }

    #[test]
    fn signatures_verify_with_ring() {
        use ring::signature::{
            RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY, RSA_PSS_2048_8192_SHA256, RsaPublicKeyComponents,
        };
        let key = Rsa::generate(2048, &BigUint::from(65537u32)).unwrap();
        let public = RsaPublicKeyComponents { n: key.n.to_bytes_be(), e: key.e.to_bytes_be() };
        let pkcs1 = key.sign("sha256", b"hello", PKCS1_PADDING, SALTLEN_MAX).unwrap();
        assert!(public.verify(&RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY, b"hello", &pkcs1).is_ok());
        let pss = key.sign("sha256", b"hello", PSS_PADDING, SALTLEN_DIGEST).unwrap();
        assert!(public.verify(&RSA_PSS_2048_8192_SHA256, b"hello", &pss).is_ok());
    }
}
