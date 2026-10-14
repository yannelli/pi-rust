//! Streaming AEAD ciphers (AES-GCM, ChaCha20-Poly1305) over ring, plus scrypt and HKDF.

use ring::aead::{AES_128_GCM, AES_256_GCM, Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};

use super::{HashState, pbkdf2};
use crate::error::JsError;

fn auth_failure() -> JsError {
    JsError::new("Unsupported state or unable to authenticate data")
}

/// An AEAD cipher fed in chunks. `update` XORs with keystream taken from sealing zeros; the
/// tag comes from sealing the whole plaintext at `finish`.
pub(super) struct Aead {
    key: LessSafeKey,
    nonce: [u8; 12],
    decrypt: bool,
    tag_length: Option<usize>,
    aad: Vec<u8>,
    plaintext: Vec<u8>,
    keystream: Vec<u8>,
    expected_tag: Option<Vec<u8>>,
}

impl Aead {
    pub(super) fn new(
        name: &str,
        key: &[u8],
        iv: &[u8],
        decrypt: bool,
        tag_length: Option<usize>,
    ) -> Result<Self, JsError> {
        let (algorithm, chacha) = match name {
            "aes-128-gcm" | "id-aes128-gcm" => (&AES_128_GCM, false),
            "aes-256-gcm" | "id-aes256-gcm" => (&AES_256_GCM, false),
            "chacha20-poly1305" => (&CHACHA20_POLY1305, true),
            _ => return Err(JsError::new("Unknown cipher").with_code("ERR_CRYPTO_UNKNOWN_CIPHER")),
        };
        if key.len() != algorithm.key_len() {
            return Err(JsError::range_error("Invalid key length").with_code("ERR_CRYPTO_INVALID_KEYLEN"));
        }
        let nonce: [u8; 12] = match iv.try_into() {
            Ok(nonce) => nonce,
            Err(_) if !chacha && !iv.is_empty() => {
                return Err(JsError::unsupported("crypto AES-GCM with an IV other than 12 bytes"));
            }
            Err(_) => {
                return Err(JsError::type_error("Invalid initialization vector").with_code("ERR_CRYPTO_INVALID_IV"));
            }
        };
        if let Some(length) = tag_length {
            let valid = if chacha { (1..=16).contains(&length) } else { matches!(length, 4 | 8 | 12..=16) };
            if !valid {
                return Err(JsError::type_error(format!("Invalid authentication tag length: {length}"))
                    .with_code("ERR_CRYPTO_INVALID_AUTH_TAG"));
            }
        }
        let key =
            LessSafeKey::new(UnboundKey::new(algorithm, key).map_err(|_| JsError::range_error("Invalid key length"))?);
        Ok(Self {
            key,
            nonce,
            decrypt,
            tag_length,
            aad: Vec::new(),
            plaintext: Vec::new(),
            keystream: Vec::new(),
            expected_tag: None,
        })
    }

    pub(super) fn set_aad(&mut self, aad: &[u8]) {
        self.aad.extend_from_slice(aad);
    }

    pub(super) fn set_tag(&mut self, tag: &[u8]) -> Result<(), JsError> {
        if !self.decrypt || !(4..=16).contains(&tag.len()) {
            return Err(JsError::type_error(format!("Invalid authentication tag length: {}", tag.len()))
                .with_code("ERR_CRYPTO_INVALID_AUTH_TAG"));
        }
        self.expected_tag = Some(tag.to_vec());
        Ok(())
    }

    fn seal(&self, data: &mut [u8], aad: &[u8]) -> Result<Vec<u8>, JsError> {
        let nonce = Nonce::assume_unique_for_key(self.nonce);
        let tag = self.key.seal_in_place_separate_tag(nonce, Aad::from(aad), data).map_err(|_| auth_failure())?;
        Ok(tag.as_ref().to_vec())
    }

    pub(super) fn update(&mut self, input: &[u8]) -> Result<Vec<u8>, JsError> {
        let start = self.plaintext.len();
        let needed = start + input.len();
        if self.keystream.len() < needed {
            let mut zeros = vec![0; needed.max(2 * self.keystream.len()).max(256)];
            self.seal(&mut zeros, &[])?;
            self.keystream = zeros;
        }
        let output: Vec<u8> = input.iter().zip(&self.keystream[start..needed]).map(|(byte, key)| byte ^ key).collect();
        self.plaintext.extend_from_slice(if self.decrypt { &output } else { input });
        Ok(output)
    }

    /// Finish and return the authentication tag; decryption checks it against `set_tag`.
    pub(super) fn finish(&mut self) -> Result<Vec<u8>, JsError> {
        let mut data = std::mem::take(&mut self.plaintext);
        let aad = std::mem::take(&mut self.aad);
        let mut tag = self.seal(&mut data, &aad)?;
        if !self.decrypt {
            tag.truncate(self.tag_length.unwrap_or(16));
            return Ok(tag);
        }
        let expected = self.expected_tag.as_ref().ok_or_else(auth_failure)?;
        let difference = expected.iter().zip(&tag).fold(0u8, |acc, (a, b)| acc | (a ^ b));
        if difference != 0 || self.tag_length.is_some_and(|length| length != expected.len()) {
            return Err(auth_failure());
        }
        Ok(Vec::new())
    }
}

fn salsa8(block: &mut [u32; 16]) {
    let mut x = *block;
    let quarter = |x: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize| {
        x[b] ^= x[a].wrapping_add(x[d]).rotate_left(7);
        x[c] ^= x[b].wrapping_add(x[a]).rotate_left(9);
        x[d] ^= x[c].wrapping_add(x[b]).rotate_left(13);
        x[a] ^= x[d].wrapping_add(x[c]).rotate_left(18);
    };
    for _ in 0..4 {
        quarter(&mut x, 0, 4, 8, 12);
        quarter(&mut x, 5, 9, 13, 1);
        quarter(&mut x, 10, 14, 2, 6);
        quarter(&mut x, 15, 3, 7, 11);
        quarter(&mut x, 0, 1, 2, 3);
        quarter(&mut x, 5, 6, 7, 4);
        quarter(&mut x, 10, 11, 8, 9);
        quarter(&mut x, 15, 12, 13, 14);
    }
    for (word, mixed) in block.iter_mut().zip(x) {
        *word = word.wrapping_add(mixed);
    }
}

fn block_mix(input: &[u32], output: &mut [u32], r: usize) {
    let mut x: [u32; 16] = input[(2 * r - 1) * 16..].try_into().unwrap_or([0; 16]);
    for i in 0..2 * r {
        x.iter_mut().zip(&input[i * 16..(i + 1) * 16]).for_each(|(word, other)| *word ^= other);
        salsa8(&mut x);
        let target = (i / 2 + (i % 2) * r) * 16;
        output[target..target + 16].copy_from_slice(&x);
    }
}

fn ro_mix(block: &mut [u8], n: usize, r: usize) {
    let words = 32 * r;
    let mut x: Vec<u32> =
        block.chunks_exact(4).map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])).collect();
    let mut table = vec![0u32; n * words];
    let mut scratch = vec![0u32; words];
    for i in 0..n {
        table[i * words..(i + 1) * words].copy_from_slice(&x);
        block_mix(&x, &mut scratch, r);
        std::mem::swap(&mut x, &mut scratch);
    }
    for _ in 0..n {
        let j = (x[(2 * r - 1) * 16] as usize) & (n - 1);
        x.iter_mut().zip(&table[j * words..(j + 1) * words]).for_each(|(word, other)| *word ^= other);
        block_mix(&x, &mut scratch, r);
        std::mem::swap(&mut x, &mut scratch);
    }
    for (chunk, word) in block.chunks_exact_mut(4).zip(x) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
}

/// RFC 7914 scrypt; `n` must be a power of two.
pub(super) fn scrypt(
    password: &[u8],
    salt: &[u8],
    n: usize,
    r: usize,
    p: usize,
    length: usize,
) -> Result<Vec<u8>, JsError> {
    let mut blocks = pbkdf2(password, salt, 1, p * 128 * r, "sha256")?;
    for block in blocks.chunks_exact_mut(128 * r) {
        ro_mix(block, n, r);
    }
    pbkdf2(password, &blocks, 1, length, "sha256")
}

/// RFC 5869 HKDF over the HMACs that `createHmac` supports.
pub(super) fn hkdf(hash: &str, ikm: &[u8], salt: &[u8], info: &[u8], length: usize) -> Result<Vec<u8>, JsError> {
    let hmac = |key: &[u8], parts: &[&[u8]]| -> Result<Vec<u8>, JsError> {
        let mut state = HashState::hmac(hash, key)?;
        parts.iter().for_each(|part| state.inner.update(part));
        Ok(state.finish())
    };
    let hash_length = hmac(&[], &[])?.len();
    if length > 255 * hash_length {
        return Err(JsError::range_error("Invalid key length").with_code("ERR_CRYPTO_INVALID_KEYLEN"));
    }
    let salt = if salt.is_empty() { vec![0; hash_length] } else { salt.to_vec() };
    let prk = hmac(&salt, &[ikm])?;
    let mut out = Vec::with_capacity(length);
    let mut previous = Vec::new();
    let mut counter = 1u8;
    while out.len() < length {
        previous = hmac(&prk, &[&previous, info, &[counter]])?;
        out.extend_from_slice(&previous);
        counter = counter.wrapping_add(1);
    }
    out.truncate(length);
    Ok(out)
}
