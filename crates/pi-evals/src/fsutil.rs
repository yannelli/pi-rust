//! Private directories and files, JSON number output, and SHA-256 hex digests.

use std::fs;
use std::io::Write;
use std::path::Path;

use serde::Serializer;

/// `mkdir -p` with mode 0700 on Unix.
pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

/// Write `content` to a new or truncated file with mode 0600 on Unix.
pub fn write_private(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(content)
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, data);
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A JSON number as JavaScript prints it: integral values have no fraction.
pub fn json_number(value: f64) -> serde_json::Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        if value >= 0.0 {
            return serde_json::Value::from(value as u64);
        }
        return serde_json::Value::from(value as i64);
    }
    serde_json::Value::from(value)
}

pub fn ser_f64<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(&json_number(*value), serializer)
}

pub fn ser_opt_f64<S: Serializer>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(&value.map(json_number), serializer)
}

/// Deep JSON equality that compares numbers by value, so `0` equals `0.0`.
pub fn json_equal(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| json_equal(x, y)),
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(key, x)| b.get(key).is_some_and(|y| json_equal(x, y)))
        }
        _ => left == right,
    }
}
