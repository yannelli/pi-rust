//! Session and entry ids, ISO timestamps, and path helpers shared by the session modules.

use std::hash::{BuildHasher, Hasher};
use std::path::{Component, Path, PathBuf};

const SESSION_ID_RULE: &str = "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character";

/// Reject ids that are unsafe in file names, as upstream `assertValidSessionId` does.
pub fn assert_valid_session_id(id: &str) -> anyhow::Result<()> {
    let bytes = id.as_bytes();
    let valid = !bytes.is_empty()
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid { Ok(()) } else { anyhow::bail!(SESSION_ID_RULE) }
}

fn random_u64() -> u64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    hasher.write_u128(nanos);
    hasher.finish()
}

/// Lowercase hex digits from the random source.
pub fn random_hex(length: usize) -> String {
    let mut text = String::new();
    while text.len() < length {
        text.push_str(&format!("{:016x}", random_u64()));
    }
    text.truncate(length);
    text
}

/// A random version 4 UUID string.
pub fn uuid_v4() -> String {
    let (high, low) = (random_u64(), random_u64());
    let version = 0x4000 | (high & 0x0fff) as u16;
    let variant = 0x8000 | ((high >> 16) & 0x3fff) as u16;
    format!("{:08x}-{:04x}-{version:04x}-{variant:04x}-{:012x}", high >> 32, low >> 48, low & 0xffff_ffff_ffff)
}

/// A version 7 UUID: 48-bit millisecond timestamp, then random bits.
pub fn uuid_v7() -> String {
    let millis = pi_ai::now_ms() as u64 & 0xffff_ffff_ffff;
    let (high, low) = (random_u64(), random_u64());
    let time_high = (millis >> 16) as u32;
    let time_low = (millis & 0xffff) as u16;
    let version = 0x7000 | (high & 0x0fff) as u16;
    let variant = 0x8000 | ((high >> 16) & 0x3fff) as u16;
    format!("{time_high:08x}-{time_low:04x}-{version:04x}-{variant:04x}-{:012x}", low & 0xffff_ffff_ffff)
}

/// An 8-hex entry id not in use, falling back to a full UUID like upstream `generateId`.
pub fn generate_entry_id(in_use: impl Fn(&str) -> bool) -> String {
    for _ in 0..100 {
        let id = uuid_v4()[..8].to_string();
        if !in_use(&id) {
            return id;
        }
    }
    uuid_v4()
}

/// UTC time as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn iso_timestamp() -> String {
    format_iso(pi_ai::now_ms())
}

pub fn format_iso(millis: i64) -> String {
    let seconds = millis.div_euclid(1000);
    let (days, second_of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        second_of_day / 3600,
        second_of_day % 3600 / 60,
        second_of_day % 60,
        millis.rem_euclid(1000)
    )
}

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Milliseconds for an ISO 8601 date-time (`Z`, offset, or no zone as UTC), like `Date.parse`.
pub fn parse_iso(text: &str) -> Option<i64> {
    let text = text.trim();
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    if text.as_bytes().get(4) != Some(&b'-') || text.as_bytes().get(7) != Some(&b'-') {
        return None;
    }
    let mut millis = days_from_civil(year, month, day) * 86_400_000;
    let rest = &text[10..];
    if rest.is_empty() {
        return Some(millis);
    }
    let rest = rest.strip_prefix(['T', ' '])?;
    let (hour, minute) = (rest.get(0..2)?.parse::<i64>().ok()?, rest.get(3..5)?.parse::<i64>().ok()?);
    let mut rest = &rest[5..];
    let mut seconds = 0;
    if let Some(after) = rest.strip_prefix(':') {
        seconds = after.get(0..2)?.parse::<i64>().ok()?;
        rest = &after[2..];
    }
    let mut fraction = 0;
    if let Some(after) = rest.strip_prefix('.') {
        let digits = after.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(after.len());
        let padded = format!("{:0<3}", &after[..digits.min(3)]);
        fraction = padded.parse::<i64>().ok()?;
        rest = &after[digits..];
    }
    millis += ((hour * 60 + minute) * 60 + seconds) * 1000 + fraction;
    let offset = match rest {
        "" | "Z" | "z" => 0,
        zone => {
            let sign = match zone.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let zone = zone[1..].replace(':', "");
            let hours = zone.get(0..2)?.parse::<i64>().ok()?;
            let minutes = zone.get(2..4).map_or(Some(0), |text| text.parse::<i64>().ok())?;
            sign * (hours * 60 + minutes) * 60_000
        }
    };
    Some(millis - offset)
}

/// An absolute, lexically normalized path, like Node's `path.resolve`.
pub fn resolve_path(path: &Path) -> PathBuf {
    let absolute =
        if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(path) };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// `path` resolved against `base` when relative.
pub fn resolve_path_from(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() { resolve_path(path) } else { resolve_path(&base.join(path)) }
}

/// Length in UTF-16 code units, which is what JavaScript's `String.length` reports.
pub fn js_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses_timestamps() {
        assert_eq!(format_iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_iso(1_760_000_000_123), "2025-10-09T08:53:20.123Z");
        assert_eq!(parse_iso("2025-10-09T08:53:20.123Z"), Some(1_760_000_000_123));
        assert_eq!(parse_iso("2025-01-01T00:00:00Z"), Some(1_735_689_600_000));
        assert_eq!(parse_iso("2025-01-01T02:00:00+02:00"), Some(1_735_689_600_000));
        assert_eq!(parse_iso("garbage"), None);
    }

    #[test]
    fn generates_ids() {
        let id = uuid_v7();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "7");
        assert_ne!(uuid_v7(), id);
        assert_eq!(generate_entry_id(|_| false).len(), 8);
        assert_eq!(generate_entry_id(|_| true).len(), 36);
    }

    #[test]
    fn validates_session_ids() {
        for id in ["abc", "abc-123_def.456", "a"] {
            assert!(assert_valid_session_id(id).is_ok(), "{id}");
        }
        for id in ["", "-abc", "abc-", "_abc", "abc_", ".abc", "abc.", "abc/def", "abc\\def", "abc def"] {
            let error = assert_valid_session_id(id).unwrap_err().to_string();
            assert!(error.starts_with("Session id must be non-empty, contain only alphanumeric characters"));
        }
    }
}
