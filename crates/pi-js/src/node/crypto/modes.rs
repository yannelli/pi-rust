//! Block ciphers (AES, DES, 3DES) and the unauthenticated modes over them: CBC and ECB with
//! optional PKCS#7 padding, and the CTR, CFB, and OFB stream modes.

use aes::cipher::consts::{U8, U16};
use aes::cipher::{
    Array, BlockCipherDecClosure, BlockCipherDecrypt, BlockCipherEncClosure, BlockCipherEncrypt, BlockModeDecrypt,
    BlockModeEncrypt, BlockSizeUser, InnerIvInit, KeyInit, StreamCipher, StreamCipherCoreWrapper,
};

use aes::cipher::common::InnerInit;

use crate::error::JsError;

/// Wrap a set of block ciphers with one block size in an enum that dispatches per call,
/// so each mode is compiled once per block size instead of once per key size.
macro_rules! any_cipher {
    ($name:ident, $size:ty, $($variant:ident($ty:ty)),+) => {
        #[derive(Clone)]
        pub(super) enum $name {
            $($variant($ty)),+
        }

        impl BlockSizeUser for $name {
            type BlockSize = $size;
        }

        impl BlockCipherEncrypt for $name {
            fn encrypt_with_backend(&self, f: impl BlockCipherEncClosure<BlockSize = $size>) {
                match self {
                    $(Self::$variant(cipher) => cipher.encrypt_with_backend(f)),+
                }
            }
        }

        impl BlockCipherDecrypt for $name {
            fn decrypt_with_backend(&self, f: impl BlockCipherDecClosure<BlockSize = $size>) {
                match self {
                    $(Self::$variant(cipher) => cipher.decrypt_with_backend(f)),+
                }
            }
        }
    };
}

any_cipher!(AnyAes, U16, A128(aes::Aes128), A192(aes::Aes192), A256(aes::Aes256));
any_cipher!(AnyDes, U8, Single(des::Des), Ede2(des::TdesEde2), Ede3(des::TdesEde3));

pub(super) fn invalid_key() -> JsError {
    JsError::range_error("Invalid key length").with_code("ERR_CRYPTO_INVALID_KEYLEN")
}

pub(super) fn invalid_iv() -> JsError {
    JsError::type_error("Invalid initialization vector").with_code("ERR_CRYPTO_INVALID_IV")
}

impl AnyAes {
    pub(super) fn new(key: &[u8]) -> Result<Self, JsError> {
        Ok(match key.len() {
            16 => Self::A128(aes::Aes128::new_from_slice(key).map_err(|_| invalid_key())?),
            24 => Self::A192(aes::Aes192::new_from_slice(key).map_err(|_| invalid_key())?),
            32 => Self::A256(aes::Aes256::new_from_slice(key).map_err(|_| invalid_key())?),
            _ => return Err(invalid_key()),
        })
    }
}

impl AnyDes {
    /// DES (8-byte key), two-key 3DES (16), or three-key 3DES (24); `key_len` is the expected size.
    pub(super) fn new(key: &[u8], key_len: usize) -> Result<Self, JsError> {
        if key.len() != key_len {
            return Err(invalid_key());
        }
        Ok(match key_len {
            8 => Self::Single(des::Des::new_from_slice(key).map_err(|_| invalid_key())?),
            16 => Self::Ede2(des::TdesEde2::new_from_slice(key).map_err(|_| invalid_key())?),
            _ => Self::Ede3(des::TdesEde3::new_from_slice(key).map_err(|_| invalid_key())?),
        })
    }
}

/// A mode that transforms whole blocks and keeps its chaining state between calls.
pub(super) trait BlockProcess {
    fn process(&mut self, data: &mut [u8]);
}

struct Enc<T>(T);
struct Dec<T>(T);

impl<T: BlockModeEncrypt> BlockProcess for Enc<T> {
    fn process(&mut self, data: &mut [u8]) {
        let (blocks, _) = Array::slice_as_chunks_mut(data);
        self.0.encrypt_blocks(blocks);
    }
}

impl<T: BlockModeDecrypt> BlockProcess for Dec<T> {
    fn process(&mut self, data: &mut [u8]) {
        let (blocks, _) = Array::slice_as_chunks_mut(data);
        self.0.decrypt_blocks(blocks);
    }
}

/// A mode that encrypts any number of bytes per call.
pub(super) trait Stream {
    fn apply(&mut self, data: &mut [u8]);
}

struct Keystream<T>(T);
struct CfbEnc<C: BlockCipherEncrypt>(cfb_mode::BufEncryptor<C>);
struct CfbDec<C: BlockCipherEncrypt>(cfb_mode::BufDecryptor<C>);

impl<T: StreamCipher> Stream for Keystream<T> {
    fn apply(&mut self, data: &mut [u8]) {
        self.0.apply_keystream(data);
    }
}

impl<C: BlockCipherEncrypt> Stream for CfbEnc<C> {
    fn apply(&mut self, data: &mut [u8]) {
        self.0.encrypt(data);
    }
}

impl<C: BlockCipherEncrypt> Stream for CfbDec<C> {
    fn apply(&mut self, data: &mut [u8]) {
        self.0.decrypt(data);
    }
}

fn block_mode<C>(cipher: C, mode: &str, iv: &[u8], decrypt: bool) -> Result<Box<dyn BlockProcess>, JsError>
where
    C: BlockCipherEncrypt + BlockCipherDecrypt + 'static,
{
    Ok(match (mode, decrypt) {
        ("cbc", false) => Box::new(Enc(cbc::Encryptor::inner_iv_slice_init(cipher, iv).map_err(|_| invalid_iv())?)),
        ("cbc", true) => Box::new(Dec(cbc::Decryptor::inner_iv_slice_init(cipher, iv).map_err(|_| invalid_iv())?)),
        (_, false) => Box::new(Enc(ecb::Encryptor::inner_init(cipher))),
        (_, true) => Box::new(Dec(ecb::Decryptor::inner_init(cipher))),
    })
}

/// A CFB or OFB stream over `$cipher`; a macro since the wrappers need a concrete block size.
macro_rules! stream_mode {
    ($cipher:expr, $mode:expr, $iv:expr, $decrypt:expr) => {{
        let bad = |_| invalid_iv();
        let stream: Box<dyn Stream> = match ($mode, $decrypt) {
            ("ofb", _) => {
                let core = ofb::OfbCore::inner_iv_slice_init($cipher, $iv).map_err(bad)?;
                Box::new(Keystream(StreamCipherCoreWrapper::from_core(core)))
            }
            ("cfb", false) => Box::new(CfbEnc(cfb_mode::BufEncryptor::inner_iv_slice_init($cipher, $iv).map_err(bad)?)),
            _ => Box::new(CfbDec(cfb_mode::BufDecryptor::inner_iv_slice_init($cipher, $iv).map_err(bad)?)),
        };
        Ok(stream)
    }};
}

/// The block size and a CBC or ECB mode for an AES or DES `family` (key length for DES).
pub(super) fn blocks(
    family: Family,
    mode: &str,
    key: &[u8],
    iv: &[u8],
    decrypt: bool,
) -> Result<(usize, Box<dyn BlockProcess>), JsError> {
    if mode == "ecb" && !iv.is_empty() {
        return Err(invalid_iv());
    }
    match family {
        Family::Aes(len) if key.len() == len => Ok((16, block_mode(AnyAes::new(key)?, mode, iv, decrypt)?)),
        Family::Des(len) => Ok((8, block_mode(AnyDes::new(key, len)?, mode, iv, decrypt)?)),
        Family::Aes(_) => Err(invalid_key()),
    }
}

/// A CTR (AES only), CFB, or OFB stream.
pub(super) fn stream(
    family: Family,
    mode: &str,
    key: &[u8],
    iv: &[u8],
    decrypt: bool,
) -> Result<Box<dyn Stream>, JsError> {
    match family {
        Family::Aes(len) if key.len() != len => Err(invalid_key()),
        Family::Aes(_) if mode == "ctr" => {
            let core = ctr::CtrCore::<AnyAes, ctr::flavors::Ctr128BE>::inner_iv_slice_init(AnyAes::new(key)?, iv)
                .map_err(|_| invalid_iv())?;
            Ok(Box::new(Keystream(StreamCipherCoreWrapper::from_core(core))))
        }
        Family::Aes(_) => stream_mode!(AnyAes::new(key)?, mode, iv, decrypt),
        Family::Des(len) => stream_mode!(AnyDes::new(key, len)?, mode, iv, decrypt),
    }
}

/// AES with a key length, or DES with a key length of 8, 16, or 24 bytes.
#[derive(Clone, Copy)]
pub(super) enum Family {
    Aes(usize),
    Des(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn modes_match_sp800_38a_first_blocks() {
        let key = unhex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = unhex("000102030405060708090a0b0c0d0e0f");
        let plain = unhex("6bc1bee22e409f96e93d7e117393172a");
        let (_, mut cbc) = blocks(Family::Aes(16), "cbc", &key, &iv, false).unwrap();
        let mut data = plain.clone();
        cbc.process(&mut data);
        assert_eq!(data, unhex("7649abac8119b246cee98e9b12e9197d"));
        let counter = unhex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff");
        let mut data = plain.clone();
        stream(Family::Aes(16), "ctr", &key, &counter, false).unwrap().apply(&mut data);
        assert_eq!(data, unhex("874d6191b620e3261bef6864990db6ce"));
    }
}
