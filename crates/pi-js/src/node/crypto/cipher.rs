//! Streaming `Cipheriv`/`Decipheriv` state: OpenSSL cipher names mapped to the AEAD, block, and
//! stream modes in `aead` and `modes`.

use super::aead::{self, Sealer, invalid_tag};
use super::modes::{self, BlockProcess, Family, Stream};
use crate::error::JsError;

fn auth_failure() -> JsError {
    JsError::new("Unsupported state or unable to authenticate data")
}

fn unknown_cipher() -> JsError {
    JsError::new("Unknown cipher").with_code("ERR_CRYPTO_UNKNOWN_CIPHER")
}

fn invalid_state(operation: &str) -> JsError {
    JsError::new(format!("Invalid state for operation {operation}")).with_code("ERR_CRYPTO_INVALID_STATE")
}

fn wrong_final_block() -> JsError {
    JsError::new("error:1C80006B:Provider routines::wrong final block length")
        .with_code("ERR_OSSL_WRONG_FINAL_BLOCK_LENGTH")
}

pub(super) fn bad_decrypt() -> JsError {
    JsError::new("error:1C800064:Provider routines::bad decrypt").with_code("ERR_OSSL_BAD_DECRYPT")
}

enum Kind {
    Blocks(Family, &'static str),
    Stream(Family, &'static str),
    /// An AEAD mode and its key length.
    Aead(&'static str, usize),
}

const MODES: [&str; 8] = ["cbc", "ecb", "ctr", "cfb", "ofb", "gcm", "ccm", "ocb"];

fn kind(family: Family, mode: &'static str) -> Kind {
    match (mode, family) {
        ("cbc" | "ecb", _) => Kind::Blocks(family, mode),
        ("ctr" | "cfb" | "ofb", _) => Kind::Stream(family, mode),
        (_, Family::Aes(len) | Family::Des(len)) => Kind::Aead(mode, len),
    }
}

/// Map an OpenSSL cipher name (lowercase) to its kind.
fn parse(name: &str) -> Option<Kind> {
    let des = |len, mode| Some(kind(Family::Des(len), mode));
    match name {
        "chacha20-poly1305" => return Some(Kind::Aead("chacha20-poly1305", 32)),
        "des" | "des-cbc" => return des(8, "cbc"),
        "des3" => return des(24, "cbc"),
        "des-ede" | "des-ede-ecb" => return des(16, "ecb"),
        "des-ede3" | "des-ede3-ecb" => return des(24, "ecb"),
        "aes128" | "aes192" | "aes256" => return Some(kind(Family::Aes(name[3..].parse::<usize>().ok()? / 8), "cbc")),
        _ => {}
    }
    for (prefix, len) in [("des-ede3-", 24), ("des-ede-", 16), ("des-", 8)] {
        if let Some(mode) = name.strip_prefix(prefix) {
            return des(len, MODES[..5].iter().find(|known| **known == mode)?);
        }
    }
    let (bits, mode) = if let Some(rest) = name.strip_prefix("id-aes") {
        rest.split_once('-').filter(|(_, mode)| matches!(*mode, "gcm" | "ccm"))?
    } else {
        name.strip_prefix("aes-")?.split_once('-')?
    };
    let family = Family::Aes(match bits {
        "128" => 16,
        "192" => 24,
        "256" => 32,
        _ => return None,
    });
    Some(kind(family, MODES.iter().find(|known| **known == mode)?))
}

/// An AEAD fed in chunks. Keystream modes XOR `update` input with keystream taken from sealing
/// zeros and compute the tag at `finish`; OCB buffers everything until `finish`.
pub(super) struct Aead {
    sealer: Box<dyn Sealer>,
    buffered: bool,
    truncated_tag: bool,
    decrypt: bool,
    tag_length: Option<usize>,
    aad: Vec<u8>,
    data: Vec<u8>,
    keystream: Vec<u8>,
    expected_tag: Option<Vec<u8>>,
}

impl Aead {
    fn new(mode: &str, key: &[u8], iv: &[u8], decrypt: bool, tag_length: Option<usize>) -> Result<Self, JsError> {
        let truncated_tag = matches!(mode, "gcm" | "chacha20-poly1305");
        if let Some(length) = tag_length {
            let valid = match mode {
                "chacha20-poly1305" => (1..=16).contains(&length),
                "gcm" => matches!(length, 4 | 8 | 12..=16),
                "ccm" => matches!(length, 4 | 6 | 8 | 10 | 12 | 14 | 16),
                _ => (1..=16).contains(&length),
            };
            if !valid {
                return Err(invalid_tag(length));
            }
        } else if mode == "ccm" {
            return Err(
                JsError::type_error("authTagLength required for CCM ciphers").with_code("ERR_CRYPTO_INVALID_AUTH_TAG")
            );
        }
        Ok(Self {
            sealer: aead::sealer(mode, key, iv, tag_length.unwrap_or(16))?,
            buffered: mode == "ocb",
            truncated_tag,
            decrypt,
            tag_length,
            aad: Vec::new(),
            data: Vec::new(),
            keystream: Vec::new(),
            expected_tag: None,
        })
    }

    fn update(&mut self, input: &[u8]) -> Result<Vec<u8>, JsError> {
        if self.buffered {
            self.data.extend_from_slice(input);
            return Ok(Vec::new());
        }
        let start = self.data.len();
        let needed = start + input.len();
        if self.keystream.len() < needed {
            let mut zeros = vec![0; needed.max(2 * self.keystream.len()).max(256)];
            self.sealer.seal(&mut zeros, &[]).ok_or_else(auth_failure)?;
            self.keystream = zeros;
        }
        let output: Vec<u8> = input.iter().zip(&self.keystream[start..needed]).map(|(byte, key)| byte ^ key).collect();
        self.data.extend_from_slice(if self.decrypt { &output } else { input });
        Ok(output)
    }

    fn finish(&mut self) -> Result<(Vec<u8>, Vec<u8>), JsError> {
        let mut data = std::mem::take(&mut self.data);
        let aad = std::mem::take(&mut self.aad);
        if self.decrypt {
            let expected = self.expected_tag.take().ok_or_else(auth_failure)?;
            if self.tag_length.is_some_and(|length| length != expected.len()) {
                return Err(auth_failure());
            }
            if self.buffered {
                self.sealer.open(&mut data, &aad, &expected).ok_or_else(auth_failure)?;
                return Ok((data, Vec::new()));
            }
            let tag = self.sealer.seal(&mut data, &aad).ok_or_else(auth_failure)?;
            let compared = if self.truncated_tag { tag.get(..expected.len()) } else { Some(&tag[..]) };
            if !compared.is_some_and(|tag| super::ct_eq(tag, &expected)) {
                return Err(auth_failure());
            }
            return Ok((Vec::new(), Vec::new()));
        }
        let mut tag = self.sealer.seal(&mut data, &aad).ok_or_else(auth_failure)?;
        tag.truncate(self.tag_length.unwrap_or(16));
        Ok((if self.buffered { data } else { Vec::new() }, tag))
    }
}

pub(super) enum Cipher {
    Aead(Aead),
    Blocks { mode: Box<dyn BlockProcess>, size: usize, decrypt: bool, padding: bool, pending: Vec<u8> },
    Stream(Box<dyn Stream>),
}

impl Cipher {
    pub(super) fn new(
        name: &str,
        key: &[u8],
        iv: &[u8],
        decrypt: bool,
        tag_length: Option<usize>,
    ) -> Result<Self, JsError> {
        Ok(match parse(&name.to_ascii_lowercase()).ok_or_else(unknown_cipher)? {
            Kind::Aead(_, len) if key.len() != len => return Err(modes::invalid_key()),
            Kind::Aead(mode, _) => Self::Aead(Aead::new(mode, key, iv, decrypt, tag_length)?),
            Kind::Blocks(family, mode) => {
                let (size, mode) = modes::blocks(family, mode, key, iv, decrypt)?;
                Self::Blocks { mode, size, decrypt, padding: true, pending: Vec::new() }
            }
            Kind::Stream(family, mode) => Self::Stream(modes::stream(family, mode, key, iv, decrypt)?),
        })
    }

    pub(super) fn set_aad(&mut self, aad: &[u8]) -> Result<(), JsError> {
        match self {
            Self::Aead(aead) => {
                aead.aad.extend_from_slice(aad);
                Ok(())
            }
            _ => Err(invalid_state("setAAD")),
        }
    }

    pub(super) fn set_tag(&mut self, tag: &[u8]) -> Result<(), JsError> {
        match self {
            Self::Aead(aead) if aead.decrypt && (1..=16).contains(&tag.len()) => {
                aead.expected_tag = Some(tag.to_vec());
                Ok(())
            }
            Self::Aead(_) => Err(invalid_tag(tag.len())),
            _ => Err(invalid_state("setAuthTag")),
        }
    }

    pub(super) fn set_padding(&mut self, enabled: bool) {
        if let Self::Blocks { padding, .. } = self {
            *padding = enabled;
        }
    }

    pub(super) fn update(&mut self, input: &[u8]) -> Result<Vec<u8>, JsError> {
        match self {
            Self::Aead(aead) => aead.update(input),
            Self::Stream(stream) => {
                let mut data = input.to_vec();
                stream.apply(&mut data);
                Ok(data)
            }
            Self::Blocks { mode, size, decrypt, padding, pending } => {
                pending.extend_from_slice(input);
                let mut ready = pending.len() / *size * *size;
                // Decryption with padding holds back the last full block for `finish`.
                if *decrypt && *padding && ready == pending.len() && ready > 0 {
                    ready -= *size;
                }
                let mut data: Vec<u8> = pending.drain(..ready).collect();
                mode.process(&mut data);
                Ok(data)
            }
        }
    }

    /// The remaining output and, for AEAD encryption, the authentication tag.
    pub(super) fn finish(&mut self) -> Result<(Vec<u8>, Vec<u8>), JsError> {
        match self {
            Self::Aead(aead) => aead.finish(),
            Self::Stream(_) => Ok((Vec::new(), Vec::new())),
            Self::Blocks { mode, size, decrypt, padding, pending } => {
                let mut data = std::mem::take(pending);
                let size = *size;
                if !*padding {
                    // Whole blocks already went out from `update`.
                    return if data.is_empty() { Ok((data, Vec::new())) } else { Err(wrong_final_block()) };
                }
                if *decrypt {
                    if data.len() != size {
                        return Err(wrong_final_block());
                    }
                    mode.process(&mut data);
                    let length = unpad(&data, size).ok_or_else(bad_decrypt)?;
                    data.truncate(length);
                    return Ok((data, Vec::new()));
                }
                let fill = size - data.len() % size;
                data.resize(data.len() + fill, fill as u8);
                mode.process(&mut data);
                Ok((data, Vec::new()))
            }
        }
    }
}

/// The unpadded length of a PKCS#7-padded final block, checked without early exits.
fn unpad(block: &[u8], size: usize) -> Option<usize> {
    let pad = *block.last()? as usize;
    let mut bad = u8::from(pad == 0 || pad > size);
    for (index, byte) in block.iter().rev().enumerate() {
        let inside = u8::from(index < pad);
        bad |= inside & u8::from(*byte as usize != pad);
    }
    (bad == 0).then(|| block.len() - pad)
}
