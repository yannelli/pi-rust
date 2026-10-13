//! PKCE (RFC 7636) and random values for OAuth.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom, SystemRandom};

/// A code verifier and its S256 challenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    SystemRandom::new().fill(&mut bytes).expect("the system random number generator works");
    bytes
}

/// 32 random bytes as hex, for the OAuth `state` parameter.
pub fn random_state() -> String {
    random_bytes::<32>().iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, data).as_ref().to_vec()
}

pub fn base64_url(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

/// The S256 challenge of a verifier.
pub fn challenge_for(verifier: &str) -> String {
    base64_url(&sha256(verifier.as_bytes()))
}

pub fn generate() -> Pkce {
    let verifier = base64_url(&random_bytes::<32>());
    Pkce { challenge: challenge_for(&verifier), verifier }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_is_the_base64url_sha256_of_the_verifier() {
        // RFC 7636 appendix B.
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let pkce = generate();
        assert_eq!(pkce.verifier.len(), 43);
        assert_eq!(pkce.challenge, challenge_for(&pkce.verifier));
        assert_eq!(random_state().len(), 64);
    }
}
