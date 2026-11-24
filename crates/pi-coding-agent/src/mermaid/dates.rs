//! Calendar time for Gantt charts: fractional days since 1970-01-01 UTC, parsed with a
//! dayjs-style `dateFormat` and printed with a d3-style `axisFormat`.

const SECS_PER_DAY: f64 = 86_400.0;
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
/// Longest first, so `YYYY` wins over `YY` and `MM` over `M`.
const TOKENS: [&str; 15] = ["YYYY", "SSS", "YY", "MM", "DD", "HH", "mm", "ss", "M", "D", "H", "m", "s", "X", "x"];

/// Days from 1970-01-01 to a proleptic Gregorian date (Hinnant's algorithm).
pub(super) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The `(year, month, day)` of a day number.
pub(super) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Day of the week, 0 for Sunday.
pub(super) fn weekday(day: i64) -> usize {
    (day + 4).rem_euclid(7) as usize
}

/// Weekday index for an English day name such as `monday`.
pub(super) fn weekday_named(name: &str) -> Option<usize> {
    WEEKDAYS.iter().position(|day| day.eq_ignore_ascii_case(name))
}

/// Parse `input` strictly against a dayjs format (`YYYY-MM-DD`, `DD.MM.YY HH:mm`, `X`).
pub(super) fn parse_date(input: &str, format: &str) -> Option<f64> {
    let input = input.as_bytes();
    let (mut year, mut month, mut day) = (1970_i64, 1_u32, 1_u32);
    let mut secs = 0.0;
    let (mut i, mut j) = (0, 0);
    while j < format.len() {
        let Some(token) = TOKENS.iter().find(|t| format[j..].starts_with(**t)) else {
            if input.get(i) != Some(&format.as_bytes()[j]) {
                return None;
            }
            i += 1;
            j += 1;
            continue;
        };
        j += token.len();
        if matches!(*token, "X" | "x") {
            let text = std::str::from_utf8(&input[i..]).ok()?;
            let value: f64 = text.parse().ok().filter(|v: &f64| v.is_finite())?;
            let per_day = if *token == "X" { SECS_PER_DAY } else { SECS_PER_DAY * 1000.0 };
            return (j == format.len()).then_some(value / per_day);
        }
        let (min, max) = match token.len() {
            1 => (1, 2),
            n => (n, n),
        };
        let digits = input[i..].iter().take(max).take_while(|b| b.is_ascii_digit()).count();
        if digits < min {
            return None;
        }
        let value: u32 = std::str::from_utf8(&input[i..i + digits]).ok()?.parse().ok()?;
        i += digits;
        match *token {
            "YYYY" => year = i64::from(value),
            "YY" => year = i64::from(value) + if value > 68 { 1900 } else { 2000 },
            "MM" | "M" => month = value,
            "DD" | "D" => day = value,
            "HH" | "H" if value < 24 => secs += f64::from(value) * 3600.0,
            "mm" | "m" if value < 60 => secs += f64::from(value) * 60.0,
            "ss" | "s" if value < 60 => secs += f64::from(value),
            "SSS" => secs += f64::from(value) / 1000.0,
            _ => return None,
        }
    }
    if i != input.len() || !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(days_from_civil(year, month, day) as f64 + secs / SECS_PER_DAY)
}

/// A Mermaid duration such as `10d`, `1.5h` or `2w`, in days. Months and years count
/// as 30 and 365 days.
pub(super) fn parse_duration(s: &str) -> Option<f64> {
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (number, unit) = s.split_at(split);
    if number.is_empty() || number.starts_with('.') || number.ends_with('.') {
        return None;
    }
    let value: f64 = number.parse().ok()?;
    let per_unit = match unit {
        "ms" => 1.0 / (SECS_PER_DAY * 1000.0),
        "s" => 1.0 / SECS_PER_DAY,
        "m" => 1.0 / 1440.0,
        "h" => 1.0 / 24.0,
        "d" => 1.0,
        "w" => 7.0,
        "M" => 30.0,
        "y" => 365.0,
        _ => return None,
    };
    Some(value * per_unit)
}

/// A `tickInterval` such as `1week` or `6hour`, in days.
pub(super) fn parse_interval(s: &str) -> Option<f64> {
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = s.split_at(split);
    let value: f64 = number.parse().ok().filter(|v| *v > 0.0)?;
    let per_unit = match unit {
        "millisecond" => 1.0 / (SECS_PER_DAY * 1000.0),
        "second" => 1.0 / SECS_PER_DAY,
        "minute" => 1.0 / 1440.0,
        "hour" => 1.0 / 24.0,
        "day" => 1.0,
        "week" => 7.0,
        "month" => 30.0,
        _ => return None,
    };
    Some(value * per_unit)
}

/// Print `t` with d3 `strftime` directives; an unknown directive prints as written.
pub(super) fn format_date(t: f64, format: &str) -> String {
    let total = (t * SECS_PER_DAY).round() as i64;
    let day = total.div_euclid(86_400);
    let secs = total.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(day);
    let (hour, minute, second) = (secs / 3600, secs / 60 % 60, secs % 60);
    let mut out = String::with_capacity(format.len() + 8);
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&y.to_string()),
            Some('y') => out.push_str(&format!("{:02}", y.rem_euclid(100))),
            Some('m') => out.push_str(&format!("{m:02}")),
            Some('d') => out.push_str(&format!("{d:02}")),
            Some('e') => out.push_str(&format!("{d:>2}")),
            Some('H') => out.push_str(&format!("{hour:02}")),
            Some('I') => out.push_str(&format!("{:02}", (hour + 11) % 12 + 1)),
            Some('p') => out.push_str(if hour < 12 { "AM" } else { "PM" }),
            Some('M') => out.push_str(&format!("{minute:02}")),
            Some('S') => out.push_str(&format!("{second:02}")),
            Some('j') => out.push_str(&format!("{:03}", day - days_from_civil(y, 1, 1) + 1)),
            Some('b') => out.push_str(&MONTHS[m as usize - 1][..3]),
            Some('B') => out.push_str(MONTHS[m as usize - 1]),
            Some('a') => out.push_str(&WEEKDAYS[weekday(day)][..3]),
            Some('A') => out.push_str(WEEKDAYS[weekday(day)]),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}
