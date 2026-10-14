//! DSA keys over the RustCrypto `dsa` crate: parameter and key generation, signing, verification.
//! Integers travel as big-endian bytes; signatures as fixed-size `r || s`.

use dsa::{BoxedUint, Components, KeySize, SigningKey, VerifyingKey};

use super::der::fixed;
use crate::error::JsError;

#[derive(Clone)]
pub(super) struct Dsa {
    pub p: Vec<u8>,
    pub q: Vec<u8>,
    pub g: Vec<u8>,
    pub y: Vec<u8>,
    pub x: Option<Vec<u8>>,
}

fn invalid() -> JsError {
    JsError::new("error:1E08010C:DECODER routines::unsupported").with_code("ERR_OSSL_UNSUPPORTED")
}

fn rng_failed(_: getrandom::Error) -> JsError {
    JsError::new("The system random generator failed")
}

fn trimmed(value: &BoxedUint) -> Vec<u8> {
    fixed(&value.to_be_bytes(), 0)
}

/// A number padded to `size` bytes so related values share one precision.
fn number(bytes: &[u8], size: usize) -> BoxedUint {
    BoxedUint::from_be_slice_vartime(&fixed(bytes, size))
}

impl Dsa {
    /// Generate parameters and a key pair; OpenSSL picks `divisor` from `modulus` when it is unset.
    pub(super) fn generate(modulus: u64, divisor: Option<u64>) -> Result<Self, JsError> {
        let divisor = divisor.unwrap_or(match modulus {
            ..2048 => 160,
            2048 => 224,
            _ => 256,
        });
        #[allow(deprecated)]
        let size = match (modulus, divisor) {
            (1024, 160) => KeySize::DSA_1024_160,
            (2048, 224) => KeySize::DSA_2048_224,
            (2048, 256) => KeySize::DSA_2048_256,
            (3072, 256) => KeySize::DSA_3072_256,
            _ => {
                return Err(JsError::unsupported(&format!(
                    "crypto DSA keys with modulusLength {modulus} and divisorLength {divisor}"
                )));
            }
        };
        let rng = &mut getrandom::SysRng;
        let components = Components::try_generate_from_rng_with_key_size(rng, size).map_err(rng_failed)?;
        let key = SigningKey::try_generate_from_rng_with_components(rng, components).map_err(rng_failed)?;
        let public = key.verifying_key();
        let parts = public.components();
        Ok(Self {
            p: trimmed(parts.p()),
            q: trimmed(parts.q()),
            g: trimmed(parts.g()),
            y: trimmed(public.y()),
            x: Some(trimmed(key.x())),
        })
    }

    /// A private key from the domain parameters and `x`, with `y = g^x mod p` in constant time.
    pub(super) fn from_private(p: &[u8], q: &[u8], g: &[u8], x: &[u8]) -> Result<Self, JsError> {
        use crypto_bigint::Odd;
        use crypto_bigint::modular::{BoxedMontyForm, BoxedMontyParams};
        let size = fixed(p, 0).len();
        let modulus = Odd::new(number(p, size)).into_option().ok_or_else(invalid)?;
        let params = BoxedMontyParams::new_vartime(modulus);
        let y = BoxedMontyForm::new(number(g, size), &params).pow(&number(x, fixed(q, 0).len())).retrieve();
        let key = Self { p: p.to_vec(), q: q.to_vec(), g: g.to_vec(), y: trimmed(&y), x: Some(x.to_vec()) };
        key.verifying_key().map(|_| key)
    }

    /// Byte length of `q`, `x`, and each half of a signature.
    pub(super) fn scalar_size(&self) -> usize {
        fixed(&self.q, 0).len()
    }

    fn verifying_key(&self) -> Result<VerifyingKey, JsError> {
        let size = fixed(&self.p, 0).len();
        let q = number(&self.q, self.scalar_size());
        let components = Components::from_components_unchecked(number(&self.p, size), q, number(&self.g, size))
            .map_err(|_| invalid())?;
        VerifyingKey::from_components(components, number(&self.y, size)).map_err(|_| invalid())
    }

    pub(super) fn modulus_bits(&self) -> usize {
        let p = fixed(&self.p, 0);
        p.len() * 8 - p.first().map_or(8, |byte| byte.leading_zeros() as usize)
    }

    pub(super) fn divisor_bits(&self) -> usize {
        let q = fixed(&self.q, 0);
        q.len() * 8 - q.first().map_or(8, |byte| byte.leading_zeros() as usize)
    }

    /// A signature `r || s` over a message digest, with a random nonce.
    pub(super) fn sign(&self, digest: &[u8]) -> Result<Vec<u8>, JsError> {
        use dsa::signature::hazmat::RandomizedPrehashSigner;
        let x = self.x.as_ref().ok_or_else(invalid)?;
        let key =
            SigningKey::from_components(self.verifying_key()?, number(x, self.scalar_size())).map_err(|_| invalid())?;
        let signature = key
            .sign_prehash_with_rng(&mut getrandom::SysRng, digest)
            .map_err(|_| JsError::new("DSA signing failed").with_code("ERR_CRYPTO_OPERATION_FAILED"))?;
        let size = self.scalar_size();
        Ok([fixed(&signature.r().to_be_bytes(), size), fixed(&signature.s().to_be_bytes(), size)].concat())
    }

    pub(super) fn verify(&self, digest: &[u8], signature: &[u8]) -> bool {
        use dsa::signature::hazmat::PrehashVerifier;
        let size = self.scalar_size();
        let Ok(key) = self.verifying_key() else { return false };
        if signature.len() != 2 * size {
            return false;
        }
        let (r, s) = signature.split_at(size);
        dsa::Signature::from_components(number(r, size), number(s, size))
            .is_some_and(|signature| key.verify_prehash(digest, &signature).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_signs_and_verifies() {
        let key = Dsa::generate(1024, None).unwrap();
        assert_eq!((key.modulus_bits(), key.divisor_bits()), (1024, 160));
        let digest = ring::digest::digest(&ring::digest::SHA256, b"hello");
        let signature = key.sign(digest.as_ref()).unwrap();
        assert!(key.verify(digest.as_ref(), &signature));
        let other = ring::digest::digest(&ring::digest::SHA256, b"hellO");
        assert!(!key.verify(other.as_ref(), &signature));
        assert!(Dsa::generate(1000, None).is_err());
    }
}
