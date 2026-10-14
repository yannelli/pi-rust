//! Natives for keys, signatures, ciphers, Diffie-Hellman, primes, scrypt, and HKDF.
//! Byte arguments arrive as latin1 strings; results return as `Uint8Array`.

use num_bigint::BigUint;
use rquickjs::function::Opt;
use rquickjs::{Array, Ctx, Object, Value};
use serde_json::Value as Json;

use super::bigint::{default_rounds, generate_prime, is_probable_prime, modp_group, to_fixed};
use super::cipher::Cipher;
use super::keys::Key;
use super::ops::{self, SignOptions};
use super::{ec, kdf, normalize_hash};
use crate::error::JsError;
use crate::node::set_fn;
use crate::runtime::convert::{bytes_to_js, latin1_to_bytes as bytes, throw};
use crate::runtime::state::state;

fn out<'js>(ctx: &Ctx<'js>, result: Result<Vec<u8>, JsError>) -> rquickjs::Result<Value<'js>> {
    result.map_err(|error| throw(ctx, &error)).and_then(|bytes| bytes_to_js(ctx, bytes))
}

fn check<T>(ctx: &Ctx<'_>, result: Result<T, JsError>) -> rquickjs::Result<T> {
    result.map_err(|error| throw(ctx, &error))
}

/// A stored key: PKCS#8 when `private`, SPKI otherwise.
fn stored(private: bool, der: &str) -> Result<Key, JsError> {
    Key::parse("der", if private { "pkcs8" } else { "spki" }, &bytes(der), None)
}

fn json(text: &str) -> Json {
    serde_json::from_str(text).unwrap_or(Json::Null)
}

fn sign_options(options: &Json) -> Result<SignOptions<'static>, JsError> {
    let hash = match options.get("hash").and_then(Json::as_str) {
        Some(name) if !name.is_empty() => normalize_hash(name)?,
        _ => "",
    };
    Ok(SignOptions {
        hash,
        padding: options.get("padding").and_then(Json::as_i64).unwrap_or(0) as i32,
        salt_length: options.get("saltLength").and_then(Json::as_i64).map(|length| length as i32),
        p1363: options.get("dsaEncoding").and_then(Json::as_str) == Some("ieee-p1363"),
    })
}

fn curve(name: &str) -> Result<&'static ec::Curve, JsError> {
    ec::curve(name).ok_or_else(ec::invalid_curve)
}

fn with_cipher<T>(id: u64, f: impl FnOnce(&mut Cipher) -> Result<T, JsError>) -> Result<T, JsError> {
    state().with_resource(id, |cipher: &mut Cipher| f(cipher))?
}

/// Diffie-Hellman `verifyError` flags for a prime and generator.
fn dh_check(p: &BigUint, g: &BigUint) -> Result<u32, JsError> {
    let rounds = 20;
    if !is_probable_prime(p, rounds)? {
        return Ok(1);
    }
    let mut flags = 0;
    if g <= &BigUint::from(1u8) || g >= &(p - 1u8) {
        flags |= 8;
    }
    if !is_probable_prime(&(p >> 1u32), rounds)? {
        flags |= 2;
    }
    Ok(flags)
}

pub(super) fn register<'js>(object: &Object<'js>) -> rquickjs::Result<()> {
    set_fn(object, "normalizeHash", |ctx: Ctx<'js>, name: String| {
        check(&ctx, normalize_hash(&name)).map(str::to_string)
    })?;
    set_fn(
        object,
        "keyParse",
        |ctx: Ctx<'js>,
         kind: String,
         format: String,
         der_type: String,
         data: String,
         passphrase: Opt<Option<String>>| {
            let passphrase = passphrase.0.flatten().map(|text| bytes(&text));
            let result =
                Key::parse(&format, &der_type, &bytes(&data), passphrase.as_deref()).and_then(|key| {
                    match kind.as_str() {
                        "private" if key.is_private() => key.pkcs8(),
                        "private" => Err(super::der::unsupported_key()),
                        _ => Ok(key.spki()),
                    }
                });
            out(&ctx, result)
        },
    )?;
    set_fn(object, "keyInfo", |ctx: Ctx<'js>, private: bool, der: String| {
        check(
            &ctx,
            stored(private, &der)
                .map(|key| serde_json::json!({ "type": key.type_name(), "details": key.details() }).to_string()),
        )
    })?;
    set_fn(
        object,
        "keyExport",
        |ctx: Ctx<'js>,
         private: bool,
         der: String,
         format: String,
         der_type: String,
         cipher: Opt<Option<String>>,
         passphrase: Opt<Option<String>>| {
            let (cipher, passphrase) = (cipher.0.flatten(), bytes(&passphrase.0.flatten().unwrap_or_default()));
            let cipher = cipher.as_deref().map(|cipher| (cipher, passphrase.as_slice()));
            out(&ctx, stored(private, &der).and_then(|key| key.export(private, &format, &der_type, cipher)))
        },
    )?;
    set_fn(object, "keyGenerate", |ctx: Ctx<'js>, kind: String, options: String| {
        out(&ctx, ops::generate(&kind, &json(&options)).and_then(|key| key.pkcs8()))
    })?;
    set_fn(object, "sign", |ctx: Ctx<'js>, private: bool, der: String, data: String, options: String| {
        let result = sign_options(&json(&options))
            .and_then(|options| ops::sign(&stored(private, &der)?, &bytes(&data), &options));
        out(&ctx, result)
    })?;
    set_fn(
        object,
        "verify",
        |ctx: Ctx<'js>, private: bool, der: String, data: String, signature: String, options: String| {
            let result = sign_options(&json(&options))
                .and_then(|options| ops::verify(&stored(private, &der)?, &bytes(&data), &bytes(&signature), &options));
            check(&ctx, result)
        },
    )?;
    set_fn(
        object,
        "rsaCrypt",
        |ctx: Ctx<'js>, op: String, private: bool, der: String, data: String, options: String| {
            let options = json(&options);
            let padding = options.get("padding").and_then(Json::as_i64).unwrap_or(1) as i32;
            let hash = options.get("oaepHash").and_then(Json::as_str).unwrap_or_default();
            let label = bytes(options.get("oaepLabel").and_then(Json::as_str).unwrap_or_default());
            let result = (if hash.is_empty() { Ok("") } else { normalize_hash(hash) })
                .and_then(|hash| ops::rsa_crypt(&op, &stored(private, &der)?, &bytes(&data), padding, hash, &label));
            out(&ctx, result)
        },
    )?;
    set_fn(object, "keyAgree", |ctx: Ctx<'js>, private: String, public: String| {
        out(&ctx, stored(true, &private).and_then(|private| ops::agree(&private, &stored(false, &public)?)))
    })?;
    set_fn(object, "ecGenerate", |ctx: Ctx<'js>, name: String| out(&ctx, curve(&name).map(|curve| curve.generate())))?;
    set_fn(object, "ecPublic", |ctx: Ctx<'js>, name: String, private: String| {
        out(&ctx, curve(&name).and_then(|curve| curve.public(&bytes(&private))))
    })?;
    set_fn(object, "ecCompute", |ctx: Ctx<'js>, name: String, private: String, public: String| {
        out(&ctx, curve(&name).and_then(|curve| curve.ecdh(&bytes(&private), &bytes(&public))))
    })?;
    set_fn(object, "ecConvert", |ctx: Ctx<'js>, name: String, public: String, form: u8| {
        out(&ctx, curve(&name).and_then(|curve| curve.convert(&bytes(&public), form)))
    })?;
    set_fn(
        object,
        "cipherNew",
        |ctx: Ctx<'js>, name: String, key: String, iv: String, decrypt: bool, tag: Option<usize>| {
            check(
                &ctx,
                Cipher::new(&name, &bytes(&key), &bytes(&iv), decrypt, tag)
                    .map(|cipher| state().insert_resource(Box::new(cipher))),
            )
        },
    )?;
    set_fn(object, "cipherAad", |ctx: Ctx<'js>, id: u64, aad: String| {
        check(&ctx, with_cipher(id, |cipher| cipher.set_aad(&bytes(&aad))))
    })?;
    set_fn(object, "cipherTag", |ctx: Ctx<'js>, id: u64, tag: String| {
        check(&ctx, with_cipher(id, |cipher| cipher.set_tag(&bytes(&tag))))
    })?;
    set_fn(object, "cipherPadding", |ctx: Ctx<'js>, id: u64, enabled: bool| {
        check(
            &ctx,
            with_cipher(id, |cipher| {
                cipher.set_padding(enabled);
                Ok(())
            }),
        )
    })?;
    set_fn(object, "cipherUpdate", |ctx: Ctx<'js>, id: u64, data: String| {
        out(&ctx, with_cipher(id, |cipher| cipher.update(&bytes(&data))))
    })?;
    set_fn(object, "cipherFinal", |ctx: Ctx<'js>, id: u64| -> rquickjs::Result<Array<'js>> {
        let result = with_cipher(id, Cipher::finish);
        state().resources.borrow_mut().remove(&id);
        let (data, tag) = check(&ctx, result)?;
        let array = Array::new(ctx.clone())?;
        array.set(0, bytes_to_js(&ctx, data)?)?;
        array.set(1, bytes_to_js(&ctx, tag)?)?;
        Ok(array)
    })?;
    set_fn(
        object,
        "scrypt",
        |ctx: Ctx<'js>, password: String, salt: String, n: usize, r: usize, p: usize, length: usize| {
            out(&ctx, kdf::scrypt(&bytes(&password), &bytes(&salt), n, r, p, length))
        },
    )?;
    set_fn(object, "hkdf", |ctx: Ctx<'js>, hash: String, ikm: String, salt: String, info: String, length: usize| {
        out(
            &ctx,
            normalize_hash(&hash).and_then(|hash| kdf::hkdf(hash, &bytes(&ikm), &bytes(&salt), &bytes(&info), length)),
        )
    })?;
    set_fn(object, "generatePrime", |ctx: Ctx<'js>, size: u32, safe: bool, add: String, rem: String| {
        let add = (!add.is_empty()).then(|| BigUint::from_bytes_be(&bytes(&add)));
        let rem = (!rem.is_empty()).then(|| BigUint::from_bytes_be(&bytes(&rem)));
        out(&ctx, generate_prime(u64::from(size), safe, add.as_ref(), rem.as_ref()).map(|prime| prime.to_bytes_be()))
    })?;
    set_fn(object, "checkPrime", |ctx: Ctx<'js>, candidate: String, checks: u32| {
        let candidate = BigUint::from_bytes_be(&bytes(&candidate));
        let rounds = if checks == 0 { default_rounds(candidate.bits()) } else { checks };
        check(&ctx, is_probable_prime(&candidate, rounds))
    })?;
    set_fn(object, "dhGroup", |ctx: Ctx<'js>, name: String| {
        out(
            &ctx,
            modp_group(&name)
                .map(|prime| prime.to_bytes_be())
                .ok_or_else(|| JsError::new("Unknown DH group").with_code("ERR_CRYPTO_UNKNOWN_DH_GROUP")),
        )
    })?;
    set_fn(object, "dhCheck", |ctx: Ctx<'js>, prime: String, generator: String| {
        check(&ctx, dh_check(&BigUint::from_bytes_be(&bytes(&prime)), &BigUint::from_bytes_be(&bytes(&generator))))
    })?;
    set_fn(object, "modPow", |ctx: Ctx<'js>, base: String, exponent: String, modulus: String| {
        let modulus = BigUint::from_bytes_be(&bytes(&modulus));
        if modulus == BigUint::default() {
            return out(&ctx, Err(JsError::new("Invalid modulus")));
        }
        let size = modulus.bits().div_ceil(8) as usize;
        let value = BigUint::from_bytes_be(&bytes(&base)).modpow(&BigUint::from_bytes_be(&bytes(&exponent)), &modulus);
        out(&ctx, Ok(to_fixed(&value, size)))
    })?;
    Ok(())
}
