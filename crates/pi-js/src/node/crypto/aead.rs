//! One-shot AEAD primitives behind one trait: AES-GCM and ChaCha20-Poly1305 over ring, and
//! AES-GCM (other key or IV sizes), AES-CCM, and AES-OCB over the RustCrypto crates.

use aes::cipher::array::ArraySize;
use aes::cipher::consts::{U1, U2, U3, U4, U5, U6, U7, U8, U9, U10, U11, U12, U13, U14, U15, U16, U32, U64};
use aes_gcm::aead::{AeadCore, AeadInOut, Nonce, Tag};
use ring::aead::{AES_128_GCM, AES_256_GCM, Aad, CHACHA20_POLY1305, LessSafeKey, UnboundKey};

use super::modes::{AnyAes, invalid_iv, invalid_key};
use crate::error::JsError;

/// Seal returns the full tag for the configured tag size; open checks a tag of that size.
pub(super) trait Sealer {
    fn seal(&self, data: &mut [u8], aad: &[u8]) -> Option<Vec<u8>>;
    fn open(&self, data: &mut [u8], aad: &[u8], tag: &[u8]) -> Option<()>;
}

struct Ring {
    key: LessSafeKey,
    nonce: [u8; 12],
}

impl Sealer for Ring {
    fn seal(&self, data: &mut [u8], aad: &[u8]) -> Option<Vec<u8>> {
        let nonce = ring::aead::Nonce::assume_unique_for_key(self.nonce);
        let tag = self.key.seal_in_place_separate_tag(nonce, Aad::from(aad), data).ok()?;
        Some(tag.as_ref().to_vec())
    }

    fn open(&self, data: &mut [u8], aad: &[u8], tag: &[u8]) -> Option<()> {
        let nonce = ring::aead::Nonce::assume_unique_for_key(self.nonce);
        let mut sealed = [&data[..], tag].concat();
        let plain = self.key.open_in_place(nonce, Aad::from(aad), &mut sealed).ok()?;
        data.copy_from_slice(plain);
        Some(())
    }
}

struct Crate<A: AeadCore> {
    cipher: A,
    nonce: Nonce<A>,
}

impl<A: AeadInOut> Sealer for Crate<A> {
    fn seal(&self, data: &mut [u8], aad: &[u8]) -> Option<Vec<u8>> {
        self.cipher.encrypt_inout_detached(&self.nonce, aad, data.into()).ok().map(|tag| tag.to_vec())
    }

    fn open(&self, data: &mut [u8], aad: &[u8], tag: &[u8]) -> Option<()> {
        let tag = Tag::<A>::try_from(tag).ok()?;
        self.cipher.decrypt_inout_detached(&self.nonce, aad, data.into(), &tag).ok()
    }
}

fn boxed<A>(cipher: A, iv: &[u8]) -> Result<Box<dyn Sealer>, JsError>
where
    A: AeadInOut + 'static,
{
    let nonce = Nonce::<A>::try_from(iv).map_err(|_| invalid_iv())?;
    Ok(Box::new(Crate { cipher, nonce }))
}

fn unsupported(what: &str) -> JsError {
    JsError::unsupported(&format!("crypto {what}"))
}

/// AES-GCM, AES-CCM, AES-OCB, or ChaCha20-Poly1305; `tag` is the CCM or OCB tag length.
pub(super) fn sealer(mode: &str, key: &[u8], iv: &[u8], tag: usize) -> Result<Box<dyn Sealer>, JsError> {
    let ring = |algorithm: &'static ring::aead::Algorithm| -> Result<Box<dyn Sealer>, JsError> {
        let nonce = iv.try_into().map_err(|_| invalid_iv())?;
        let key = UnboundKey::new(algorithm, key).map_err(|_| invalid_key())?;
        Ok(Box::new(Ring { key: LessSafeKey::new(key), nonce }))
    };
    match mode {
        "chacha20-poly1305" if key.len() != 32 => Err(invalid_key()),
        "chacha20-poly1305" => ring(&CHACHA20_POLY1305),
        "gcm" if iv.len() == 12 && key.len() == 16 => ring(&AES_128_GCM),
        "gcm" if iv.len() == 12 && key.len() == 32 => ring(&AES_256_GCM),
        "gcm" => gcm(AnyAes::new(key)?, iv),
        "ccm" => ccm(AnyAes::new(key)?, iv, tag),
        _ => ocb(AnyAes::new(key)?, iv, tag),
    }
}

fn gcm(cipher: AnyAes, iv: &[u8]) -> Result<Box<dyn Sealer>, JsError> {
    macro_rules! sizes {
        ($($len:literal => $n:ty),+) => {
            match iv.len() {
                $($len => boxed(aes_gcm::AesGcm::<AnyAes, $n>::from(cipher), iv),)+
                0 => Err(invalid_iv()),
                _ => Err(unsupported("AES-GCM IVs longer than 16 bytes other than 32 or 64")),
            }
        };
    }
    sizes!(1 => U1, 2 => U2, 3 => U3, 4 => U4, 5 => U5, 6 => U6, 7 => U7, 8 => U8, 9 => U9, 10 => U10,
        11 => U11, 13 => U13, 14 => U14, 15 => U15, 16 => U16, 32 => U32, 64 => U64, 12 => U12)
}

fn ccm(cipher: AnyAes, iv: &[u8], tag: usize) -> Result<Box<dyn Sealer>, JsError> {
    fn with_nonce<N: ArraySize + ccm::NonceSize + 'static>(
        cipher: AnyAes,
        iv: &[u8],
        tag: usize,
    ) -> Result<Box<dyn Sealer>, JsError> {
        match tag {
            4 => boxed(ccm::Ccm::<AnyAes, U4, N>::from(cipher), iv),
            6 => boxed(ccm::Ccm::<AnyAes, U6, N>::from(cipher), iv),
            8 => boxed(ccm::Ccm::<AnyAes, U8, N>::from(cipher), iv),
            10 => boxed(ccm::Ccm::<AnyAes, U10, N>::from(cipher), iv),
            12 => boxed(ccm::Ccm::<AnyAes, U12, N>::from(cipher), iv),
            14 => boxed(ccm::Ccm::<AnyAes, U14, N>::from(cipher), iv),
            16 => boxed(ccm::Ccm::<AnyAes, U16, N>::from(cipher), iv),
            _ => Err(invalid_tag(tag)),
        }
    }
    match iv.len() {
        7 => with_nonce::<U7>(cipher, iv, tag),
        8 => with_nonce::<U8>(cipher, iv, tag),
        9 => with_nonce::<U9>(cipher, iv, tag),
        10 => with_nonce::<U10>(cipher, iv, tag),
        11 => with_nonce::<U11>(cipher, iv, tag),
        12 => with_nonce::<U12>(cipher, iv, tag),
        13 => with_nonce::<U13>(cipher, iv, tag),
        _ => Err(invalid_iv()),
    }
}

fn ocb(cipher: AnyAes, iv: &[u8], tag: usize) -> Result<Box<dyn Sealer>, JsError> {
    macro_rules! tags {
        ($n:ty) => {
            match tag {
                16 => boxed(ocb3::Ocb3::<AnyAes, $n, U16>::from(cipher), iv),
                12 => boxed(ocb3::Ocb3::<AnyAes, $n, U12>::from(cipher), iv),
                8 => boxed(ocb3::Ocb3::<AnyAes, $n, U8>::from(cipher), iv),
                _ => Err(unsupported("AES-OCB tags other than 8, 12, or 16 bytes")),
            }
        };
    }
    match iv.len() {
        12 => tags!(U12),
        15 => tags!(U15),
        8 => tags!(U8),
        1..=15 => Err(unsupported("AES-OCB IVs other than 8, 12, or 15 bytes")),
        _ => Err(invalid_iv()),
    }
}

pub(super) fn invalid_tag(length: usize) -> JsError {
    JsError::type_error(format!("Invalid authentication tag length: {length}")).with_code("ERR_CRYPTO_INVALID_AUTH_TAG")
}
