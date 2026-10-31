//! ISO 8601 UTC times for reset instants, without a date crate.

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

/// `2026-10-30T17:00:00Z` for Unix seconds.
pub fn iso_from_unix(seconds: i64) -> String {
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let time = seconds.rem_euclid(86_400);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", time / 3600, time / 60 % 60, time % 60)
}

/// Unix seconds of an ISO 8601 time with `Z`, an offset such as `+02:00`, or no zone (UTC).
pub fn unix_from_iso(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = text.split_once(['T', ' '])?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let zone_at = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
    let (clock, zone) = rest.split_at(zone_at);
    let clock = clock.split('.').next()?;
    let mut parts = clock.split(':').map(str::parse::<i64>);
    let (hour, minute) = (parts.next()?.ok()?, parts.next()?.ok()?);
    let second = parts.next().transpose().ok()?.unwrap_or(0);
    let offset = match zone.as_bytes().first() {
        None | Some(b'Z' | b'z') => 0,
        Some(sign) => {
            let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
            let (hours, minutes) =
                (digits.get(..2)?.parse::<i64>().ok()?, digits.get(2..4).unwrap_or("0").parse::<i64>().ok()?);
            let minutes = hours * 60 + minutes;
            if *sign == b'-' { -minutes } else { minutes }
        }
    };
    let valid = plausible_year(year).is_some()
        && (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && (0..24).contains(&hour)
        && (0..60).contains(&minute)
        && (0..61).contains(&second);
    valid.then(|| days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset * 60)
}

/// `Some` for a year that the civil-date math handles without overflow.
pub(crate) fn plausible_year(year: i64) -> Option<()> {
    (1970..=9999).contains(&year).then_some(())
}

/// `header` when its IMF-fixdate fields are in range, so the shared HTTP date parser cannot overflow.
pub(crate) fn bounded_http_date(header: &str) -> Option<&str> {
    let parts: Vec<&str> = header.split_whitespace().collect();
    let [_, day, _, year, clock, _] = parts.as_slice() else { return None };
    let clock: Vec<i64> = clock.split(':').map(|part| part.parse().ok()).collect::<Option<_>>()?;
    let [hour, minute, second] = clock.as_slice() else { return None };
    let day: i64 = day.parse().ok()?;
    plausible_year(year.parse().ok()?)?;
    let valid =
        (1..=31).contains(&day) && (0..24).contains(hour) && (0..60).contains(minute) && (0..61).contains(second);
    valid.then_some(header)
}

/// A whole number of milliseconds that fits an exact `f64` and an `i64`.
pub(crate) fn ms_from_f64(ms: f64) -> Option<i64> {
    (ms.is_finite() && ms.abs() < 9.0e15).then_some(ms as i64)
}

/// Unix milliseconds of a reset time: an ISO 8601 string (with its fraction) or a number of
/// seconds (below 10^11) or milliseconds.
pub(crate) fn ms_from_value(value: &serde_json::Value) -> Option<i64> {
    if let Some(number) = value.as_f64() {
        return ms_from_f64(if number < 1e11 { number * 1000.0 } else { number });
    }
    let text = value.as_str()?;
    let seconds = unix_from_iso(text)?;
    let fraction = text.split_once(['T', ' ']).and_then(|(_, rest)| rest.split_once('.')).map(|(_, rest)| rest);
    let digits: String = fraction.unwrap_or_default().chars().take_while(char::is_ascii_digit).take(3).collect();
    let millis = if digits.is_empty() { 0 } else { format!("{digits:0<3}").parse::<i64>().ok()? };
    seconds.checked_mul(1000)?.checked_add(millis)
}

/// Unix seconds or milliseconds; values below 10^10 are seconds.
pub(crate) fn iso_from_epoch(value: f64) -> Option<String> {
    let seconds = if value.abs() < 1e10 { value } else { value / 1000.0 };
    (seconds.is_finite() && seconds > 0.0).then(|| iso_from_unix(seconds as i64))
}

/// A time from the API normalized to `YYYY-MM-DDTHH:MM:SSZ`.
pub(crate) fn normalize(text: &str) -> Option<String> {
    unix_from_iso(text).map(iso_from_unix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_offsets() {
        assert_eq!(iso_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_unix(1_793_379_600), "2026-10-30T17:00:00Z");
        assert_eq!(unix_from_iso("2026-10-30T17:00:00Z"), Some(1_793_379_600));
        assert_eq!(unix_from_iso("2026-10-30T19:00:00.123456+02:00"), Some(1_793_379_600));
        assert_eq!(unix_from_iso("2026-10-30T12:00:00-05:00"), Some(1_793_379_600));
        assert_eq!(unix_from_iso("2028-02-29T00:00:00"), Some(days_from_civil(2028, 2, 29) * 86_400));
        assert_eq!(iso_from_unix(days_from_civil(2028, 2, 29) * 86_400), "2028-02-29T00:00:00Z");
        assert_eq!(unix_from_iso("soon"), None);
        assert_eq!(iso_from_epoch(1_793_379_600_000.0).as_deref(), Some("2026-10-30T17:00:00Z"));
        assert_eq!(normalize("2026-10-30T17:00:00.5+00:00").as_deref(), Some("2026-10-30T17:00:00Z"));
    }
}
