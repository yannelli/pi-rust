//! scrypt (RFC 7914) and HKDF (RFC 5869) over the hashes and HMACs in this module.

use super::{HashState, pbkdf2};
use crate::error::JsError;

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
