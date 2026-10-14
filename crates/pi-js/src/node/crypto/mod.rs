//! Natives for `node:crypto`: hashes and HMACs, random bytes, KDFs, ciphers, and asymmetric keys.

mod bigint;
mod cipher;
mod der;
mod ec;
mod keys;
mod natives_keys;
mod ops;
mod rsa;

use md5::Digest as _;
use ring::rand::{SecureRandom, SystemRandom};
use rquickjs::{Ctx, Object, Value};

use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{bytes_to_js, latin1_to_bytes, throw};
use crate::runtime::state::state;

#[derive(Clone)]
enum Hasher {
    Ring(ring::digest::Context),
    Md5(md5::Md5),
}

impl Hasher {
    fn new(algorithm: &str) -> Result<Self, JsError> {
        let ring = |algorithm| Ok(Self::Ring(ring::digest::Context::new(algorithm)));
        match algorithm.to_ascii_lowercase().replace('-', "").as_str() {
            "md5" => Ok(Self::Md5(md5::Md5::new())),
            "sha1" => ring(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY),
            "sha256" => ring(&ring::digest::SHA256),
            "sha384" => ring(&ring::digest::SHA384),
            "sha512" => ring(&ring::digest::SHA512),
            "sha512256" | "sha512/256" => ring(&ring::digest::SHA512_256),
            _ => Err(JsError::new("Digest method not supported").with_code("ERR_CRYPTO_INVALID_DIGEST")),
        }
    }

    fn block_size(&self) -> usize {
        match self {
            Self::Ring(context) => context.algorithm().block_len(),
            Self::Md5(_) => 64,
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Ring(context) => context.update(bytes),
            Self::Md5(hasher) => hasher.update(bytes),
        }
    }

    fn finish(self) -> Vec<u8> {
        match self {
            Self::Ring(context) => context.finish().as_ref().to_vec(),
            Self::Md5(hasher) => hasher.finalize().to_vec(),
        }
    }
}

/// A hash, or an HMAC when `outer` holds the outer hasher already fed with the padded key.
#[derive(Clone)]
struct HashState {
    inner: Hasher,
    outer: Option<Hasher>,
}

impl HashState {
    fn hash(algorithm: &str) -> Result<Self, JsError> {
        Ok(Self { inner: Hasher::new(algorithm)?, outer: None })
    }

    fn hmac(algorithm: &str, key: &[u8]) -> Result<Self, JsError> {
        let mut inner = Hasher::new(algorithm)?;
        let mut outer = inner.clone();
        let block = inner.block_size();
        let mut key = if key.len() > block {
            let mut hasher = inner.clone();
            hasher.update(key);
            hasher.finish()
        } else {
            key.to_vec()
        };
        key.resize(block, 0);
        inner.update(&key.iter().map(|byte| byte ^ 0x36).collect::<Vec<_>>());
        outer.update(&key.iter().map(|byte| byte ^ 0x5c).collect::<Vec<_>>());
        Ok(Self { inner, outer: Some(outer) })
    }

    fn finish(self) -> Vec<u8> {
        let digest = self.inner.finish();
        match self.outer {
            Some(mut outer) => {
                outer.update(&digest);
                outer.finish()
            }
            None => digest,
        }
    }
}

fn random_bytes(length: usize) -> Result<Vec<u8>, JsError> {
    let mut bytes = vec![0; length];
    SystemRandom::new().fill(&mut bytes).map_err(|_| JsError::new("The system random generator failed"))?;
    Ok(bytes)
}

/// Node's digest name in canonical form: `RSA-SHA256`, `sha256WithRSAEncryption`, `SHA-256` -> `sha256`.
fn normalize_hash(name: &str) -> Result<&'static str, JsError> {
    let lower = name.to_ascii_lowercase();
    let lower = lower.strip_prefix("rsa-").or_else(|| lower.strip_prefix("ecdsa-with-")).unwrap_or(&lower);
    let lower = lower.strip_suffix("withrsaencryption").unwrap_or(lower);
    Ok(match lower.replace(['-', '/'], "").as_str() {
        "md5" => "md5",
        "sha1" => "sha1",
        "sha256" => "sha256",
        "sha384" => "sha384",
        "sha512" => "sha512",
        "sha512256" => "sha512-256",
        _ => return Err(JsError::type_error(format!("Invalid digest: {name}")).with_code("ERR_CRYPTO_INVALID_DIGEST")),
    })
}

fn digest(hash: &str, data: &[u8]) -> Result<Vec<u8>, JsError> {
    let mut hasher = Hasher::new(normalize_hash(hash)?)?;
    hasher.update(data);
    Ok(hasher.finish())
}

fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32, length: usize, digest: &str) -> Result<Vec<u8>, JsError> {
    let algorithm = match digest.to_ascii_lowercase().replace('-', "").as_str() {
        "sha1" => ring::pbkdf2::PBKDF2_HMAC_SHA1,
        "sha256" => ring::pbkdf2::PBKDF2_HMAC_SHA256,
        "sha384" => ring::pbkdf2::PBKDF2_HMAC_SHA384,
        "sha512" => ring::pbkdf2::PBKDF2_HMAC_SHA512,
        other => {
            return Err(JsError::type_error(format!("Invalid digest: {other}")).with_code("ERR_CRYPTO_INVALID_DIGEST"));
        }
    };
    let iterations = std::num::NonZeroU32::new(iterations)
        .ok_or_else(|| JsError::range_error("iterations must be at least 1").with_code("ERR_OUT_OF_RANGE"))?;
    let mut out = vec![0; length];
    ring::pbkdf2::derive(algorithm, iterations, salt, password, &mut out);
    Ok(out)
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    let store = |ctx: &Ctx<'js>, result: Result<HashState, JsError>| {
        result.map(|hash| state().insert_resource(Box::new(hash))).map_err(|error| throw(ctx, &error))
    };
    set_fn(&object, "hash", move |ctx: Ctx<'js>, algorithm: String| store(&ctx, HashState::hash(&algorithm)))?;
    set_fn(&object, "hmac", move |ctx: Ctx<'js>, algorithm: String, key: String| {
        store(&ctx, HashState::hmac(&algorithm, &latin1_to_bytes(&key)))
    })?;
    set_fn(&object, "update", |ctx: Ctx<'js>, id: u64, bytes: String| {
        let bytes = latin1_to_bytes(&bytes);
        state().with_resource(id, |hash: &mut HashState| hash.inner.update(&bytes)).map_err(|error| throw(&ctx, &error))
    })?;
    set_fn(&object, "copy", |ctx: Ctx<'js>, id: u64| {
        let state = state();
        let copy = state.with_resource(id, |hash: &mut HashState| hash.clone()).map_err(|error| throw(&ctx, &error))?;
        Ok::<_, rquickjs::Error>(state.insert_resource(Box::new(copy)))
    })?;
    set_fn(&object, "digest", |ctx: Ctx<'js>, id: u64| -> rquickjs::Result<Value<'js>> {
        let resource = state().resources.borrow_mut().remove(&id);
        let hash = resource.and_then(|resource| resource.downcast::<HashState>().ok()).ok_or_else(|| {
            throw(&ctx, &JsError::new("Digest already called").with_code("ERR_CRYPTO_HASH_FINALIZED"))
        })?;
        bytes_to_js(&ctx, hash.finish())
    })?;
    set_fn(&object, "drop", |id: u64| state().resources.borrow_mut().remove(&id).is_some())?;
    set_fn(&object, "randomBytes", |ctx: Ctx<'js>, length: usize| {
        random_bytes(length).map_err(|error| throw(&ctx, &error)).and_then(|bytes| bytes_to_js(&ctx, bytes))
    })?;
    set_fn(
        &object,
        "pbkdf2",
        |ctx: Ctx<'js>, password: String, salt: String, iterations: u32, length: usize, digest: String| {
            pbkdf2(&latin1_to_bytes(&password), &latin1_to_bytes(&salt), iterations, length, &digest)
                .map_err(|error| throw(&ctx, &error))
                .and_then(|bytes| bytes_to_js(&ctx, bytes))
        },
    )?;
    natives_keys::register(&object)?;
    Ok(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn hashes_and_hmacs_match_known_vectors() {
        let mut md5 = HashState::hash("md5").unwrap();
        md5.inner.update(b"abc");
        assert_eq!(hex(&md5.finish()), "900150983cd24fb0d6963f7d28e17f72");
        let mut sha1 = HashState::hash("SHA-1").unwrap();
        sha1.inner.update(b"abc");
        assert_eq!(hex(&sha1.finish()), "a9993e364706816aba3e25717850c26c9cd0d89d");
        let mut hmac = HashState::hmac("sha256", b"key").unwrap();
        hmac.inner.update(b"The quick brown fox jumps over the lazy dog");
        assert_eq!(hex(&hmac.finish()), "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8");
        let mut hmac_md5 = HashState::hmac("md5", b"key").unwrap();
        hmac_md5.inner.update(b"The quick brown fox jumps over the lazy dog");
        assert_eq!(hex(&hmac_md5.finish()), "80070713463e7749b90c2dc24911e275");
        assert!(HashState::hash("whirlpool").is_err());
    }

    #[test]
    fn derives_pbkdf2_keys() {
        let key = pbkdf2(b"password", b"salt", 1, 20, "sha1").unwrap();
        assert_eq!(hex(&key), "0c60c80f961f0e71f3a9b524af6012062fe037a6");
        assert_eq!(random_bytes(16).unwrap().len(), 16);
    }
}
