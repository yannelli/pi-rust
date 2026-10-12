//! PKCE (RFC 7636) and the random values OAuth flows send: states, nonces, and UUIDs.

use base64::Engine;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD, URL_SAFE_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};

/// A PKCE code verifier and its S256 challenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// A random 32-byte verifier (base64url, 43 characters) and its SHA-256 challenge.
pub fn generate_pkce() -> Pkce {
    let verifier = base64url_encode(&random_bytes::<32>());
    let challenge = pkce_challenge(&verifier);
    Pkce { verifier, challenge }
}

/// The S256 code challenge for `verifier`: base64url of its SHA-256 digest.
pub fn pkce_challenge(verifier: &str) -> String {
    base64url_encode(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref())
}

/// Unpadded base64url, as `btoa` output with `+/` mapped to `-_` and `=` removed.
pub fn base64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

const FORGIVING: GeneralPurposeConfig = GeneralPurposeConfig::new()
    .with_decode_padding_mode(DecodePaddingMode::Indifferent)
    .with_decode_allow_trailing_bits(true);
const STANDARD_FORGIVING: GeneralPurpose = GeneralPurpose::new(&base64::alphabet::STANDARD, FORGIVING);
const URL_SAFE_FORGIVING: GeneralPurpose = GeneralPurpose::new(&base64::alphabet::URL_SAFE, FORGIVING);

/// Decodes standard or URL-safe base64 with or without padding (JWT segments use base64url).
pub fn base64_decode_lenient(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    STANDARD_FORGIVING.decode(text).or_else(|_| URL_SAFE_FORGIVING.decode(text)).ok()
}

/// Decodes a constant that upstream stores base64-encoded, such as a public client id.
pub(crate) fn decode_constant(encoded: &str) -> String {
    let bytes = STANDARD.decode(encoded).unwrap_or_default();
    String::from_utf8(bytes).unwrap_or_default()
}

/// Cryptographically secure random bytes.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    SystemRandom::new().fill(&mut bytes).expect("system random source is available");
    bytes
}

/// Lowercase hex of `N` random bytes, as Node's `randomBytes(N).toString("hex")`.
pub fn random_hex<const N: usize>() -> String {
    random_bytes::<N>().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A random version 4 UUID in lowercase hyphenated form, as `crypto.randomUUID()`.
pub fn random_uuid() -> String {
    let mut bytes = random_bytes::<16>();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// Base64url of 32 random bytes, as Node's `randomBytes(32).toString("base64url")`.
pub fn random_base64url() -> String {
    base64url_encode(&random_bytes::<32>())
}
