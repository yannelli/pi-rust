//! Random identifiers.

use ring::rand::{SecureRandom, SystemRandom};

/// A random canonical lowercase UUIDv4.
pub fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    if SystemRandom::new().fill(&mut bytes).is_err() {
        // The system generator does not fail on supported platforms; fall back to the clock.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        bytes = nanos.to_le_bytes();
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// `count` random lowercase hex characters.
#[cfg(unix)]
pub(crate) fn random_hex(count: usize) -> String {
    random_uuid().replace('-', "").chars().take(count).collect()
}
