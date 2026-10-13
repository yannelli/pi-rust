//! String formats TypeBox registers by default. Unknown formats pass, as in TypeBox.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::LazyLock;

use regex::Regex;

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("format pattern is valid")
}

static DATE: LazyLock<Regex> = LazyLock::new(|| pattern(r"^(\d\d\d\d)-(\d\d)-(\d\d)$"));
static TIME: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^(\d\d):(\d\d):(\d\d)(?:\.\d+)?(?:([Zz])|([+-])(\d\d):(\d\d))?$"));
static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r#"(?i)^(?:[a-z0-9!#$%&'*+/=?^_`{|}~-]+(?:\.[a-z0-9!#$%&'*+/=?^_`{|}~-]+)*|"(?:[^"\\]|\\[\x20-\x7e])*")@(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*|\[(?:IPv6:[a-f0-9:]+|(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])(?:\.(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])){3})\])$"#,
    )
});
static UUID: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?i)^[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}$"));
static URI: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?i)^[a-z][a-z0-9+\-.]*:[^\s]*$"));
static HOSTNAME: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r"(?i)^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*\.?$")
});
static DURATION: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^P(?:\d+W|(?:\d+Y)?(?:\d+M)?(?:\d+D)?(?:T(?:\d+H)?(?:\d+M)?(?:\d+(?:\.\d+)?S)?)?)$"));
static JSON_POINTER: LazyLock<Regex> = LazyLock::new(|| pattern(r"^(?:/(?:[^~/]|~0|~1)*)*$"));

const DAYS: [u32; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

fn is_date(value: &str) -> bool {
    let Some(captures) = DATE.captures(value) else { return false };
    let number = |index: usize| captures[index].parse::<u32>().unwrap_or_default();
    let (year, month, day) = (number(1), number(2), number(3));
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    (1..=12).contains(&month) && day >= 1 && day <= if month == 2 && leap { 29 } else { DAYS[month as usize] }
}

fn is_time(value: &str) -> bool {
    let Some(captures) = TIME.captures(value) else { return false };
    let number = |index: usize| captures.get(index).map(|m| m.as_str().parse::<i64>().unwrap_or_default());
    let (hour, minute, second) = (number(1).unwrap_or(0), number(2).unwrap_or(0), number(3).unwrap_or(0));
    let has_zone = captures.get(4).is_some() || captures.get(5).is_some();
    if !has_zone || hour > 23 || minute > 59 || second > 60 {
        return false;
    }
    let (zone_hours, zone_minutes) = (number(6).unwrap_or(0), number(7).unwrap_or(0));
    if zone_hours > 23 || zone_minutes > 59 {
        return false;
    }
    if second < 60 {
        return true;
    }
    // A leap second must fall on 23:59:60 UTC.
    let sign = if captures.get(5).map(|m| m.as_str()) == Some("-") { -1 } else { 1 };
    let utc_minutes = (hour * 60 + minute) - sign * (zone_hours * 60 + zone_minutes);
    (utc_minutes % 1440 + 1440) % 1440 == 1439
}

fn is_date_time(value: &str) -> bool {
    let parts: Vec<&str> = value.split(['T', 't']).collect();
    parts.len() == 2 && is_date(parts[0]) && is_time(parts[1])
}

/// Whether `value` matches the named format. Unregistered formats always match.
pub(crate) fn test_format(format: &str, value: &str) -> bool {
    match format {
        "date" => is_date(value),
        "time" => is_time(value),
        "date-time" => is_date_time(value),
        "duration" => value != "P" && !value.ends_with('T') && DURATION.is_match(value),
        "email" => EMAIL.is_match(value),
        "uuid" => UUID.is_match(value),
        "ipv4" => value.parse::<Ipv4Addr>().is_ok(),
        "ipv6" => value.parse::<Ipv6Addr>().is_ok(),
        "uri" | "url" | "iri" => URI.is_match(value),
        "hostname" | "idn-hostname" => value.len() <= 253 && HOSTNAME.is_match(value),
        "regex" => Regex::new(value).is_ok(),
        "json-pointer" => JSON_POINTER.is_match(value),
        _ => true,
    }
}
