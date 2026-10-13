//! AWS Signature Version 4 request signing with `ring` HMAC-SHA256 and SHA-256.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ring::{digest, hmac};

/// AWS credentials. `expiration` is set for temporary credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub expiration: Option<SystemTime>,
}

impl Credentials {
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: None,
            expiration: None,
        }
    }

    /// True when the credentials expire within `margin` of now.
    pub fn expires_within(&self, margin: Duration) -> bool {
        self.expiration.is_some_and(|expiration| expiration <= SystemTime::now() + margin)
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("expiration", &self.expiration)
            .finish_non_exhaustive()
    }
}

/// One request to sign. Header names may use any case; `host` is derived from the URL.
pub struct SignableRequest<'a> {
    pub method: &'a str,
    pub url: &'a reqwest::Url,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

/// Headers to add to the request: `authorization`, `x-amz-date`, and the session token.
pub fn sign(
    request: &SignableRequest,
    credentials: &Credentials,
    region: &str,
    service: &str,
    time: SystemTime,
) -> Vec<(String, String)> {
    let amz_date = format_amz_date(time);
    let date = &amz_date[..8];
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .filter(|(name, _)| !matches!(name.as_str(), "authorization" | "user-agent" | "x-amz-date" | "host"))
        .collect();
    headers.push(("host".into(), host_header(request.url)));
    headers.push(("x-amz-date".into(), amz_date.clone()));
    if let Some(token) = &credentials.session_token {
        headers.retain(|(name, _)| name != "x-amz-security-token");
        headers.push(("x-amz-security-token".into(), token.clone()));
    }
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String =
        headers.iter().map(|(name, value)| format!("{name}:{}\n", normalize_header_value(value))).collect();
    let signed_headers = headers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        request.method.to_ascii_uppercase(),
        canonical_uri(request.url.path()),
        canonical_query(request.url.query().unwrap_or_default()),
        hex(digest::digest(&digest::SHA256, request.body).as_ref()),
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(digest::digest(&digest::SHA256, canonical_request.as_bytes()).as_ref())
    );
    let key = hmac::Key::new(hmac::HMAC_SHA256, &signing_key(&credentials.secret_access_key, date, region, service));
    let signature = hex(hmac::sign(&key, string_to_sign.as_bytes()).as_ref());
    let mut added = vec![
        (
            "authorization".to_string(),
            format!(
                "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
                credentials.access_key_id
            ),
        ),
        ("x-amz-date".to_string(), amz_date),
    ];
    if let Some(token) = &credentials.session_token {
        added.push(("x-amz-security-token".to_string(), token.clone()));
    }
    added
}

/// The derived SigV4 signing key for one day, region, and service.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let step = |key: &[u8], data: &str| hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data.as_bytes());
    let k_date = step(format!("AWS4{secret}").as_bytes(), date);
    let k_region = step(k_date.as_ref(), region);
    let k_service = step(k_region.as_ref(), service);
    step(k_service.as_ref(), "aws4_request").as_ref().to_vec()
}

fn host_header(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

fn normalize_header_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// RFC 3986 encoding of everything except unreserved characters.
pub fn uri_encode(input: &str) -> String {
    let mut encoded = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// The wire path with dot segments removed, each segment encoded again (non-S3 services).
fn canonical_uri(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    let trailing = path.ends_with('/') && !segments.is_empty();
    let encoded: Vec<String> = segments.iter().map(|segment| uri_encode(segment)).collect();
    format!("/{}{}", encoded.join("/"), if trailing { "/" } else { "" })
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex_digits = bytes.get(index + 1..index + 3).and_then(|pair| std::str::from_utf8(pair).ok());
        match (bytes[index], hex_digits.and_then(|digits| u8::from_str_radix(digits, 16).ok())) {
            (b'%', Some(byte)) => {
                decoded.push(byte);
                index += 3;
            }
            (byte, _) => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn canonical_query(query: &str) -> String {
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (uri_encode(&percent_decode(key)), uri_encode(&percent_decode(value)))
        })
        .collect();
    pairs.sort();
    pairs.iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join("&")
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `YYYYMMDDTHHMMSSZ` in UTC.
pub fn format_amz_date(time: SystemTime) -> String {
    let seconds = time.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let rem = seconds.rem_euclid(86_400);
    format!("{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` in UTC, as JavaScript's `Date.toISOString` writes it.
pub fn format_iso8601(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since.as_secs() as i64;
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let rem = seconds.rem_euclid(86_400);
    let (hour, minute, second, millis) = (rem / 3600, rem % 3600 / 60, rem % 60, since.subsec_millis());
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse an RFC 3339 timestamp such as `2024-05-01T12:00:00Z` or with an offset.
pub fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let text = text.trim();
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let mut rest = &text[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        rest = fraction.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "Z" | "z" | "" => 0,
        offset => {
            let sign = if offset.starts_with('-') { -1 } else { 1 };
            let hours = offset.get(1..3)?.parse::<i64>().ok()?;
            let minutes = offset.get(4..6).and_then(|m| m.parse::<i64>().ok()).unwrap_or_default();
            sign * (hours * 3600 + minutes * 60)
        }
    };
    let seconds =
        days_from_civil(year, month as u32, day as u32) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(seconds).ok().map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds))
}

#[cfg(test)]
#[path = "sigv4_tests.rs"]
mod tests;
