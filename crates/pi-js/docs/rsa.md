# RSA in node:crypto

Created: 2026-11-21. Last updated: 2026-11-21.

`crates/pi-js/src/node/crypto/rsa.rs` implements the RSA part of `node:crypto` with the RustCrypto `rsa` crate 0.10 and ring. This note records the `rsa` 0.10 rules that the code depends on and the OpenSSL check order that the code copies. Sources: the `rsa` 0.10.0 code (`src/private_key.rs`, `src/public_key.rs`, `src/pkcs1v15.rs`, `CHANGELOG.md`) and probes against Node v22.23.5 (OpenSSL 3.5.9). PR: [#261](https://github.com/yannelli/pi-rust/pull/261).

## rsa 0.10 rules

- `Pkcs1v15Encrypt` and `Pkcs1v15Sign` need the `pkcs1v15` feature. The `hazmat` feature gives `rsa_encrypt`, `rsa_decrypt_and_check`, and `new_with_exp_unchecked`.
- `new_with_exp` refuses moduli under 1024 bits. Node takes 512 bits, so `Rsa::generate` uses `new_with_exp_unchecked`.
- `PublicKey::new` refuses moduli under 1024 bits and over 8192 bits. `Rsa::from_public` uses `new_with_size_limits(.., 0..=16384)`, because 16384 bits is the Node maximum.
- `PrivateKey::from_components` refuses a prime wider than half the modulus with `Error::InvalidPrime`. Node takes such keys, so `Rsa::from_private` then tries `from_components_with_wide_primes`. The `rsa` docs advise against a retry, because the import time shows that the key is unbalanced.
- `from_components` also refuses `d >= n` and `p = q`. Node imports these keys and fails when they are used. `src/unsupported.rs` lists this difference.
- `Oaep` and `Pss` take the digest as a type. The `with_digest!` macro maps a Node hash name to the type. OAEP labels can be any bytes.
- `Pss::salt_len` is an `Option`. Verification with `None` takes any salt length, as Node does for a negative `saltLength` other than `RSA_PSS_SALTLEN_DIGEST`.
- The hazmat functions need an integer at the precision of the modulus. `rsa_decrypt` refuses an input at or above n, and `rsa_encrypt` does not check. `Rsa::below_modulus` builds the integer and does the check.
- PKCS#1 v1.5 decryption needs a ciphertext of exactly the modulus length, and it uses implicit rejection: bad padding gives synthetic plaintext. `Rsa::ciphertext` pads a short ciphertext. The synthetic bytes are the same as the OpenSSL 3.5 bytes.
- Private-key operations need an RNG for blinding. The code passes `getrandom::SysRng`, because the `getrandom` feature of `rsa` is off.

## OpenSSL check order

`privateDecrypt` (PKCS#1, OAEP, and no padding):

1. Input longer than the modulus: `ERR_OSSL_RSA_DATA_GREATER_THAN_MOD_LEN`.
2. Empty input: `ERR_OSSL_RSA_DATA_TOO_SMALL`.
3. Input at or above n: `ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_MODULUS`.
4. The padding check. Bad OAEP padding fails with `ERR_OSSL_RSA_OAEP_DECODING_ERROR`. Bad PKCS#1 padding gives synthetic plaintext.

`publicDecrypt` with PKCS#1 padding checks the decrypted block in this order:

1. The first byte is not 0: `ERR_OSSL_RSA_INVALID_PADDING`.
2. The second byte is not 1: `ERR_OSSL_RSA_BLOCK_TYPE_IS_NOT_01`. Empty input gets this error.
3. No 0 byte follows the `0xff` bytes: `ERR_OSSL_RSA_NULL_BEFORE_BLOCK_MISSING`.
4. A byte other than `0xff` comes before the 0 byte: `ERR_OSSL_RSA_BAD_FIXED_HEADER_DECRYPT`.
5. Fewer than 8 `0xff` bytes: `ERR_OSSL_RSA_BAD_PAD_BYTE_COUNT`.

Other errors:

- `RSA_NO_PADDING` encryption of input shorter than the modulus: `ERR_OSSL_RSA_DATA_TOO_SMALL_FOR_KEY_SIZE`. Longer input: `ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_KEY_SIZE`.
- PSS signing with a key too small for the digest and salt: `ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_KEY_SIZE`.
- Key generation under 512 bits: `error:1C8000AB:Provider routines::key size too small`, with no `code`. An exponent that is even or less than 3: `error:020000B2:rsa routines::pub exponent out of range`, with no `code`.

## Tests

- `rsa.rs` unit tests: generated keys, a 513-bit key, unbalanced primes, the input checks, and each `publicDecrypt` error.
- `crates/pi-js/tests/node_crypto_keys.rs`: `rsa_dh_and_primes_round_trip` and `rsa_matches_node_vectors`. The second test has a key, a ciphertext, and a signature from Node 22.23.5. Run new cases under Node 22 to get their expected values.
