//! Encrypted private keys: PKCS#8 `EncryptedPrivateKeyInfo` (PBES2 through the `pkcs8` crate),
//! and OpenSSL's legacy PEM encryption (`Proc-Type: 4,ENCRYPTED`, EVP_BytesToKey with MD5).

use md5::Digest as _;
use pkcs8::pkcs5::pbes2::{EncryptionScheme, Parameters, Pbkdf2Params};
use pkcs8::{EncryptedPrivateKeyInfoRef, PrivateKeyInfoRef};

use super::cipher::{Cipher, bad_decrypt};
use super::der::{Der, pem_encode, unsupported_key};
use super::random_bytes;
use crate::error::JsError;

/// OpenSSL's PBKDF2 iteration count for `PEM_write_bio_PKCS8PrivateKey`.
const ITERATIONS: u32 = 2048;

pub(super) fn missing_passphrase() -> JsError {
    JsError::type_error("Passphrase required for encrypted key").with_code("ERR_MISSING_PASSPHRASE")
}

fn unsupported_cipher(cipher: &str) -> JsError {
    JsError::unsupported(&format!("crypto private key encryption with {cipher}"))
}

/// Whether DER is an `EncryptedPrivateKeyInfo`, whose first field is an AlgorithmIdentifier SEQUENCE.
pub(super) fn is_encrypted_pkcs8(der: &[u8]) -> bool {
    Der::new(der).seq().is_ok_and(|inner| inner.peek() == Some(0x30))
}

pub(super) fn decrypt_pkcs8(der: &[u8], passphrase: Option<&[u8]>) -> Result<Vec<u8>, JsError> {
    let passphrase = passphrase.ok_or_else(missing_passphrase)?;
    let info = EncryptedPrivateKeyInfoRef::try_from(der).map_err(|_| unsupported_key())?;
    info.decrypt(passphrase).map(|document| document.as_bytes().to_vec()).map_err(|_| bad_decrypt())
}

/// PBES2 with PBKDF2-HMAC-SHA256, as OpenSSL 3 writes for `cipher`.
pub(super) fn encrypt_pkcs8(der: &[u8], cipher: &str, passphrase: &[u8]) -> Result<Vec<u8>, JsError> {
    let iv = random_bytes(16)?;
    let aes_iv: [u8; 16] = iv.as_slice().try_into().map_err(|_| unsupported_key())?;
    let encryption = match cipher.to_ascii_lowercase().as_str() {
        "aes-128-cbc" | "aes128" => EncryptionScheme::Aes128Cbc { iv: aes_iv },
        "aes-192-cbc" | "aes192" => EncryptionScheme::Aes192Cbc { iv: aes_iv },
        "aes-256-cbc" | "aes256" => EncryptionScheme::Aes256Cbc { iv: aes_iv },
        "des-ede3-cbc" | "des3" => {
            EncryptionScheme::DesEde3Cbc { iv: iv[..8].try_into().map_err(|_| unsupported_key())? }
        }
        _ => return Err(unsupported_cipher(cipher)),
    };
    let salt = random_bytes(16)?;
    let kdf = Pbkdf2Params::hmac_sha256(ITERATIONS, &salt).map_err(|_| unsupported_key())?.into();
    let info = PrivateKeyInfoRef::try_from(der).map_err(|_| unsupported_key())?;
    let document =
        info.encrypt_with_params(Parameters { kdf, encryption }, passphrase).map_err(|_| unsupported_key())?;
    Ok(document.as_bytes().to_vec())
}

/// Key and IV lengths of the CBC ciphers legacy PEM encryption supports.
fn legacy_sizes(cipher: &str) -> Option<(usize, usize)> {
    Some(match cipher {
        "aes-128-cbc" => (16, 16),
        "aes-192-cbc" => (24, 16),
        "aes-256-cbc" => (32, 16),
        "des-ede3-cbc" => (24, 8),
        "des-cbc" => (8, 8),
        _ => return None,
    })
}

/// OpenSSL's `EVP_BytesToKey` with MD5, one iteration, and the first 8 IV bytes as salt.
fn bytes_to_key(passphrase: &[u8], salt: &[u8], length: usize) -> Vec<u8> {
    let mut key = Vec::new();
    let mut previous = Vec::new();
    while key.len() < length {
        let mut hasher = md5::Md5::new();
        hasher.update(&previous);
        hasher.update(passphrase);
        hasher.update(salt);
        previous = hasher.finalize().to_vec();
        key.extend_from_slice(&previous);
    }
    key.truncate(length);
    key
}

fn run(cipher: &str, key: &[u8], iv: &[u8], decrypt: bool, data: &[u8]) -> Result<Vec<u8>, JsError> {
    let mut state = Cipher::new(cipher, key, iv, decrypt, None)?;
    let mut out = state.update(data)?;
    out.extend(state.finish()?.0);
    Ok(out)
}

/// Decrypt a legacy encrypted PEM body given its `DEK-Info` header value.
pub(super) fn decrypt_legacy(dek_info: &str, body: &[u8], passphrase: Option<&[u8]>) -> Result<Vec<u8>, JsError> {
    let passphrase = passphrase.ok_or_else(missing_passphrase)?;
    let (cipher, iv_hex) = dek_info.split_once(',').ok_or_else(unsupported_key)?;
    let cipher = cipher.trim().to_ascii_lowercase();
    let (key_len, iv_len) = legacy_sizes(&cipher).ok_or_else(|| unsupported_cipher(&cipher))?;
    let iv = hex_decode(iv_hex.trim()).filter(|iv| iv.len() == iv_len).ok_or_else(unsupported_key)?;
    let key = bytes_to_key(passphrase, &iv[..8], key_len);
    run(&cipher, &key, &iv, true, body).map_err(|_| bad_decrypt())
}

/// A legacy encrypted PEM block, as Node writes for PKCS#1 and SEC1 keys with a `cipher`.
pub(super) fn encrypt_legacy(label: &str, der: &[u8], cipher: &str, passphrase: &[u8]) -> Result<String, JsError> {
    let cipher = cipher.to_ascii_lowercase();
    let (key_len, iv_len) = legacy_sizes(&cipher).ok_or_else(|| unsupported_cipher(&cipher))?;
    let iv = random_bytes(iv_len)?;
    let key = bytes_to_key(passphrase, &iv[..8], key_len);
    let body = run(&cipher, &key, &iv, false, der)?;
    let iv_hex: String = iv.iter().map(|byte| format!("{byte:02X}")).collect();
    let armored = pem_encode(label, &body);
    let (first, rest) = armored.split_once('\n').unwrap_or((&armored, ""));
    Ok(format!("{first}\nProc-Type: 4,ENCRYPTED\nDEK-Info: {},{iv_hex}\n\n{rest}", cipher.to_ascii_uppercase()))
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}
