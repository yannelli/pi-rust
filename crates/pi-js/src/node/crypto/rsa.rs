//! RSA over the RustCrypto `rsa` crate, with ring signing the PKCS#1 v1.5 and digest-length PSS
//! cases it supports. Private-key operations in `rsa` run blinded on constant-time arithmetic.

use getrandom::SysRng;
use rsa::hazmat::{rsa_decrypt_and_check, rsa_encrypt};
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{BoxedUint, Oaep, Pkcs1v15Encrypt, Pkcs1v15Sign, PrivateKey, Pss, PublicKey};

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
    pub public: PublicKey,
    pub private: Option<Box<PrivateKey>>,
}

fn openssl(code: &str, reason: &str, id: &str) -> JsError {
    JsError::new(format!("error:{id}:rsa routines::{reason}")).with_code(format!("ERR_OSSL_RSA_{code}"))
}

fn too_large() -> JsError {
    openssl("DATA_TOO_LARGE_FOR_KEY_SIZE", "data too large for key size", "0200006E")
}

fn too_small() -> JsError {
    openssl("DATA_TOO_SMALL_FOR_KEY_SIZE", "data too small for key size", "0200007A")
}

fn too_large_for_modulus() -> JsError {
    openssl("DATA_TOO_LARGE_FOR_MODULUS", "data too large for modulus", "02000084")
}

fn digest_too_big() -> JsError {
    openssl("DIGEST_TOO_BIG_FOR_RSA_KEY", "digest too big for rsa key", "02000070")
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

/// Evaluates `$body`, a `Result`, with the type `$D` set to the digest that `$hash` names.
macro_rules! with_digest {
    ($hash:expr, $D:ident => $body:expr) => {
        match $hash {
            "md5" => {
                type $D = md5::Md5;
                $body
            }
            "sha1" => {
                type $D = sha1::Sha1;
                $body
            }
            "sha256" => {
                type $D = rsa::sha2::Sha256;
                $body
            }
            "sha384" => {
                type $D = rsa::sha2::Sha384;
                $body
            }
            "sha512" => {
                type $D = rsa::sha2::Sha512;
                $body
            }
            "sha512-256" => {
                type $D = rsa::sha2::Sha512_256;
                $body
            }
            hash => Err(invalid_digest(hash)),
        }
    };
}

/// PKCS#1 v1.5 signature padding with the DigestInfo prefix for `hash`.
fn pkcs1_scheme(hash: &str, hash_len: usize) -> Result<Pkcs1v15Sign, JsError> {
    let info = der::seq(&[der::seq(&[der::oid(digest_oid(hash)?), der::null()]), der::octets(&vec![0; hash_len])]);
    Ok(Pkcs1v15Sign { hash_len: Some(hash_len), prefix: info[..info.len() - hash_len].into() })
}

/// PSS padding; verification with a `None` salt length takes any salt length.
fn pss<D: rsa::sha2::Digest>(salt_len: Option<usize>) -> Pss<D> {
    Pss { blinded: true, digest: D::new(), salt_len }
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

/// A big-endian integer with the smallest precision that holds it.
fn big(bytes: &[u8]) -> BoxedUint {
    match bytes.iter().position(|byte| *byte != 0) {
        Some(start) => BoxedUint::from_be_slice_vartime(&bytes[start..]),
        None => BoxedUint::from_be_slice_vartime(&[0]),
    }
}

fn trimmed(value: &BoxedUint) -> Vec<u8> {
    value.to_be_bytes_trimmed_vartime().into_vec()
}

impl Rsa {
    pub(super) fn from_public(n: &[u8], e: &[u8]) -> Result<Self, JsError> {
        let public = PublicKey::new_with_size_limits(big(n), big(e), 0..=MAX_BITS).map_err(|_| bad_key())?;
        Ok(Self { public, private: None })
    }

    /// A private key from `n`, `e`, `d`, `p`, and `q`; the CRT values are recomputed.
    pub(super) fn from_private(n: &[u8], e: &[u8], d: &[u8], p: &[u8], q: &[u8]) -> Result<Self, JsError> {
        let (n, e, d, primes) = (big(n), big(e), big(d), vec![big(p), big(q)]);
        // `from_components` refuses a prime wider than half the modulus, and Node takes such keys.
        let private = match PrivateKey::from_components(n.clone(), e.clone(), d.clone(), primes.clone()) {
            Err(rsa::Error::InvalidPrime) => PrivateKey::from_components_with_wide_primes(n, e, d, primes),
            result => result,
        }
        .map_err(|_| bad_key())?;
        Ok(Self { public: private.to_public_key(), private: Some(Box::new(private)) })
    }

    pub(super) fn generate(bits: u64, e: u64) -> Result<Self, JsError> {
        if bits < 512 {
            return Err(JsError::new("error:1C8000AB:Provider routines::key size too small"));
        }
        if e < 3 || e % 2 == 0 {
            return Err(JsError::new("error:020000B2:rsa routines::pub exponent out of range"));
        }
        // The checked `new_with_exp` refuses moduli under 1024 bits, and Node takes 512.
        let private = PrivateKey::new_with_exp_unchecked(&mut SysRng, bits as usize, BoxedUint::from(e))
            .map_err(|error| JsError::new(format!("RSA key generation failed: {error}")))?;
        Ok(Self { public: private.to_public_key(), private: Some(Box::new(private)) })
    }

    pub(super) fn n(&self) -> Vec<u8> {
        self.public.n_bytes().into_vec()
    }

    pub(super) fn e(&self) -> Vec<u8> {
        self.public.e_bytes().into_vec()
    }

    pub(super) fn bits(&self) -> usize {
        self.public.n().bit_width_vartime() as usize
    }

    pub(super) fn exponent(&self) -> String {
        num_bigint::BigUint::from_bytes_be(&self.e()).to_string()
    }

    fn size(&self) -> usize {
        self.public.size()
    }

    /// `n, e, d, p, q, dp, dq, qinv` of a two-prime private key.
    pub(super) fn private_parts(&self) -> Option<[Vec<u8>; 8]> {
        let key = self.private.as_deref()?;
        let [p, q] = key.primes() else { return None };
        Some([
            self.n(),
            self.e(),
            trimmed(key.d()),
            trimmed(p),
            trimmed(q),
            trimmed(key.dp()?),
            trimmed(key.dq()?),
            trimmed(&key.crt_coefficient()?),
        ])
    }

    fn private_key(&self) -> Result<&PrivateKey, JsError> {
        self.private.as_deref().ok_or_else(|| JsError::type_error("A private key is required"))
    }

    fn max_salt(&self, hash_len: usize) -> Result<usize, JsError> {
        (self.bits() - 1).div_ceil(8).checked_sub(hash_len + 2).ok_or_else(too_large)
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
        if padding != PSS_PADDING {
            let scheme = pkcs1_scheme(hash, hashed.len())?;
            return key.sign_with_rng(&mut SysRng, scheme, &hashed).map_err(|_| digest_too_big());
        }
        let salt = match salt_length {
            SALTLEN_DIGEST => hashed.len(),
            length if length < 0 => self.max_salt(hashed.len())?,
            length => length as usize,
        };
        if salt > self.max_salt(hashed.len())? {
            return Err(too_large());
        }
        with_digest!(hash, D => key
            .sign_with_rng(&mut SysRng, pss::<D>(Some(salt)), &hashed)
            .map_err(|_| digest_too_big()))
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
        let salt = match salt_length {
            SALTLEN_DIGEST => Some(hashed.len()),
            length if length >= 0 => Some(length as usize),
            _ => None,
        };
        with_digest!(hash, D => Ok(self.public.verify(pss::<D>(salt), &hashed, signature).is_ok()))
    }

    /// `data` as an integer below the modulus, at the modulus's precision as the raw operations require.
    fn below_modulus(&self, data: &[u8]) -> Result<BoxedUint, JsError> {
        let n = self.public.n();
        BoxedUint::from_be_slice(data, n.bits_precision())
            .ok()
            .filter(|value| value < n.as_ref())
            .ok_or_else(too_large_for_modulus)
    }

    /// `data` left-padded to the modulus length, after the range check that OpenSSL runs before it unpads.
    fn ciphertext(&self, data: &[u8]) -> Result<Vec<u8>, JsError> {
        self.below_modulus(data)?;
        Ok(fixed(data, self.size()))
    }

    fn raw_public(&self, data: &[u8]) -> Result<Vec<u8>, JsError> {
        let value = rsa_encrypt(&self.public, &self.below_modulus(data)?).map_err(|_| too_large_for_modulus())?;
        Ok(fixed(&value.to_be_bytes(), self.size()))
    }

    fn raw_private(&self, data: &[u8]) -> Result<Vec<u8>, JsError> {
        let value = rsa_decrypt_and_check(self.private_key()?, Some(&mut SysRng), &self.below_modulus(data)?)
            .map_err(|_| too_large_for_modulus())?;
        Ok(fixed(&value.to_be_bytes(), self.size()))
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
            NO_PADDING if data.len() < self.size() => Err(too_small()),
            NO_PADDING if data.len() > self.size() => Err(too_large()),
            NO_PADDING if private => self.raw_private(data),
            NO_PADDING => self.raw_public(data),
            OAEP_PADDING if !private => with_digest!(oaep_hash, D => self
                .public
                .encrypt(&mut SysRng, Oaep::<D>::new_with_label(label), data)
                .map_err(|_| too_large())),
            PKCS1_PADDING if private => self
                .private_key()?
                .sign_with_rng(&mut SysRng, Pkcs1v15Sign::new_unprefixed(), data)
                .map_err(|_| too_large()),
            PKCS1_PADDING => self.public.encrypt(&mut SysRng, Pkcs1v15Encrypt, data).map_err(|_| too_large()),
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
            NO_PADDING | OAEP_PADDING | PKCS1_PADDING if private && data.is_empty() => {
                Err(openssl("DATA_TOO_SMALL", "data too small", "0200006F"))
            }
            NO_PADDING if private => self.raw_private(data),
            NO_PADDING => self.raw_public(data),
            OAEP_PADDING if private => {
                let key = self.private_key()?;
                let ciphertext = self.ciphertext(data)?;
                with_digest!(oaep_hash, D => key
                    .decrypt_blinded(&mut SysRng, Oaep::<D>::new_with_label(label), &ciphertext)
                    .map_err(|_| openssl("OAEP_DECODING_ERROR", "oaep decoding error", "02000079")))
            }
            // Bad padding returns synthetic plaintext (implicit rejection), as OpenSSL 3.2 and later do.
            PKCS1_PADDING if private => {
                let key = self.private_key()?;
                let ciphertext = self.ciphertext(data)?;
                key.decrypt_blinded(&mut SysRng, Pkcs1v15Encrypt, &ciphertext)
                    .map_err(|_| openssl("PKCS_DECODING_ERROR", "pkcs decoding error", "0200009F"))
            }
            PKCS1_PADDING => {
                // Block type 1 carries public data, so a plain scan is fine here.
                let em = self.raw_public(data)?;
                if em[0] != 0 {
                    return Err(openssl("INVALID_PADDING", "invalid padding", "0200008A"));
                }
                if em[1] != 1 {
                    return Err(openssl("BLOCK_TYPE_IS_NOT_01", "block type is not 01", "0200006A"));
                }
                let padded = &em[2..];
                match padded.iter().position(|byte| *byte != 0xff) {
                    None => Err(openssl("NULL_BEFORE_BLOCK_MISSING", "null before block missing", "02000071")),
                    Some(end) if padded[end] != 0 => {
                        Err(openssl("BAD_FIXED_HEADER_DECRYPT", "bad fixed header decrypt", "02000066"))
                    }
                    Some(end) if end < 8 => Err(openssl("BAD_PAD_BYTE_COUNT", "bad pad byte count", "02000067")),
                    Some(end) => Ok(padded[end + 1..].to_vec()),
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

    fn code(result: Result<Vec<u8>, JsError>) -> Option<String> {
        result.err().and_then(|error| error.code)
    }

    #[test]
    fn odd_sized_and_imported_small_keys_work() {
        let key = Rsa::generate(513, 3).unwrap();
        assert_eq!((key.bits(), key.exponent(), key.e()), (513, "3".to_string(), vec![3]));
        let public = Rsa::from_public(&key.n(), &key.e()).unwrap();
        let signature = key.sign("md5", b"hello", PSS_PADDING, 7).unwrap();
        assert!(public.verify("md5", b"hello", &signature, PSS_PADDING, SALTLEN_MAX).unwrap());
        assert!(!public.verify("md5", b"hello", &signature, PSS_PADDING, 8).unwrap());
        let too_big = key.sign("sha512", b"hello", PSS_PADDING, SALTLEN_MAX);
        assert_eq!(code(too_big).as_deref(), Some("ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_KEY_SIZE"));
        let parts = key.private_parts().unwrap();
        let imported = Rsa::from_private(&parts[0], &parts[1], &parts[2], &parts[3], &parts[4]).unwrap();
        assert_eq!(imported.private_parts().unwrap(), parts);
        let mut block = vec![0; key.size()];
        block[key.size() - 1] = 42;
        let raw = public.encrypt(false, &block, NO_PADDING, "sha1", b"").unwrap();
        assert_eq!(imported.decrypt(true, &raw, NO_PADDING, "sha1", b"").unwrap(), block);
        assert_eq!(Rsa::generate(512, 65537).unwrap().exponent(), "65537");
    }

    #[test]
    fn oaep_labels_take_any_bytes() {
        let key = Rsa::generate(1024, 65537).unwrap();
        let label = [0xff, 0x00, 0xfe];
        let sealed = key.encrypt(false, b"secret", OAEP_PADDING, "sha256", &label).unwrap();
        assert_eq!(key.decrypt(true, &sealed, OAEP_PADDING, "sha256", &label).unwrap(), b"secret");
        let wrong = key.decrypt(true, &sealed, OAEP_PADDING, "sha256", b"other").unwrap_err();
        assert_eq!(wrong.code.as_deref(), Some("ERR_OSSL_RSA_OAEP_DECODING_ERROR"));
    }

    #[test]
    fn private_keys_with_unbalanced_primes_import_and_work() {
        use super::super::bigint::generate_prime;
        let e = num_bigint::BigUint::from(65537u32);
        let (small, large, d) = loop {
            let small = generate_prime(256, false, None, None).unwrap();
            let large = generate_prime(512, false, None, None).unwrap();
            if let Some(d) = e.modinv(&((&small - 1u32) * (&large - 1u32))) {
                break (small, large, d);
            }
        };
        let n = &small * &large;
        for (p, q) in [(&small, &large), (&large, &small)] {
            let parts = [&n, &e, &d, p, q].map(|value| value.to_bytes_be());
            let key = Rsa::from_private(&parts[0], &parts[1], &parts[2], &parts[3], &parts[4]).unwrap();
            assert_eq!(key.private_parts().unwrap()[..5], parts);
            let signature = key.sign("sha256", b"hello", PKCS1_PADDING, SALTLEN_MAX).unwrap();
            assert!(key.verify("sha256", b"hello", &signature, PKCS1_PADDING, SALTLEN_MAX).unwrap());
            let sealed = key.encrypt(false, b"secret", OAEP_PADDING, "sha1", b"").unwrap();
            assert_eq!(key.decrypt(true, &sealed, OAEP_PADDING, "sha1", b"").unwrap(), b"secret");
        }
    }

    #[test]
    fn raw_input_at_or_above_the_modulus_is_rejected() {
        let key = Rsa::generate(512, 65537).unwrap();
        let block = vec![0xff; key.size()];
        for private in [false, true] {
            let error = key.decrypt(private, &block, NO_PADDING, "sha1", b"").unwrap_err();
            assert_eq!(error.code.as_deref(), Some("ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_MODULUS"));
        }
        let error = key.decrypt(false, &block, PKCS1_PADDING, "sha1", b"").unwrap_err();
        assert_eq!(error.code.as_deref(), Some("ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_MODULUS"));
    }

    #[test]
    fn keys_that_node_rejects_only_at_use_fail_at_import() {
        use num_bigint::BigUint;
        let key = Rsa::generate(512, 65537).unwrap();
        let [n, e, d, p, q, ..] = key.private_parts().unwrap();
        let int = |bytes: &[u8]| BigUint::from_bytes_be(bytes);
        let wide_d = (int(&d) + (int(&p) - 1u32) * (int(&q) - 1u32)).to_bytes_be();
        let square = (int(&p) * int(&p)).to_bytes_be();
        let mut even = n.clone();
        *even.last_mut().unwrap() &= 0xfe;
        let failures = [
            Rsa::from_private(&n, &e, &wide_d, &p, &q).err(),
            Rsa::from_private(&square, &e, &d, &p, &p).err(),
            Rsa::from_public(&even, &e).err(),
            Rsa::from_public(&[0], &e).err(),
            Rsa::from_public(&n, &[1]).err(),
            Rsa::from_public(&n, &[4]).err(),
            Rsa::from_public(&[vec![0xff; 2049], vec![0xfb]].concat(), &e).err(),
        ];
        for failure in failures {
            assert_eq!(failure.and_then(|error| error.code).as_deref(), Some("ERR_OSSL_UNSUPPORTED"));
        }
    }

    #[test]
    fn private_decryption_runs_the_openssl_input_checks() {
        let key = Rsa::generate(512, 65537).unwrap();
        for padding in [PKCS1_PADDING, OAEP_PADDING] {
            let sealed = loop {
                let sealed = key.encrypt(false, b"secret", padding, "sha1", b"").unwrap();
                if sealed[0] == 0 {
                    break sealed;
                }
            };
            assert_eq!(key.decrypt(true, &sealed[1..], padding, "sha1", b"").unwrap(), b"secret");
        }
        for padding in [NO_PADDING, PKCS1_PADDING, OAEP_PADDING] {
            let empty = key.decrypt(true, &[], padding, "sha1", b"");
            assert_eq!(code(empty).as_deref(), Some("ERR_OSSL_RSA_DATA_TOO_SMALL"));
            let above = key.decrypt(true, &vec![0xff; key.size()], padding, "sha1", b"");
            assert_eq!(code(above).as_deref(), Some("ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_MODULUS"));
        }
        for private in [false, true] {
            let short = key.encrypt(private, &[1; 10], NO_PADDING, "sha1", b"");
            assert_eq!(code(short).as_deref(), Some("ERR_OSSL_RSA_DATA_TOO_SMALL_FOR_KEY_SIZE"));
        }
    }

    #[test]
    fn public_decryption_reports_the_openssl_padding_error() {
        let key = Rsa::generate(512, 65537).unwrap();
        let block = |head: [u8; 2], fill: u8, separator: Option<usize>| {
            let mut em = vec![fill; key.size()];
            em[..2].copy_from_slice(&head);
            if let Some(index) = separator {
                em[index] = 0;
            }
            key.encrypt(true, &em, NO_PADDING, "sha1", b"").unwrap()
        };
        let cases = [
            (block([0, 2], 0xff, Some(20)), "ERR_OSSL_RSA_BLOCK_TYPE_IS_NOT_01"),
            (block([1, 1], 0xff, Some(20)), "ERR_OSSL_RSA_INVALID_PADDING"),
            (block([0, 1], 0xff, None), "ERR_OSSL_RSA_NULL_BEFORE_BLOCK_MISSING"),
            (block([0, 1], 0xff, Some(8)), "ERR_OSSL_RSA_BAD_PAD_BYTE_COUNT"),
            (block([0, 1], 0xfe, Some(20)), "ERR_OSSL_RSA_BAD_FIXED_HEADER_DECRYPT"),
            (Vec::new(), "ERR_OSSL_RSA_BLOCK_TYPE_IS_NOT_01"),
        ];
        for (data, expected) in cases {
            assert_eq!(code(key.decrypt(false, &data, PKCS1_PADDING, "sha1", b"")).as_deref(), Some(expected));
        }
        let valid = block([0, 1], 0xff, Some(20));
        assert_eq!(key.decrypt(false, &valid, PKCS1_PADDING, "sha1", b"").unwrap(), vec![0xff; key.size() - 21]);
    }

    #[test]
    fn generation_errors_match_node() {
        let small = Rsa::generate(511, 65537).err().unwrap();
        assert_eq!(
            (small.message.as_str(), small.code),
            ("error:1C8000AB:Provider routines::key size too small", None)
        );
        for e in [1, 2, 4] {
            let error = Rsa::generate(512, e).err().unwrap();
            assert_eq!(
                (error.message.as_str(), error.code),
                ("error:020000B2:rsa routines::pub exponent out of range", None)
            );
        }
    }
}
