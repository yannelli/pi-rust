//! Finite JSON numbers with JavaScript `Number.prototype.toString` formatting.

use std::fmt;

/// A finite IEEE-754 double, the only number type JSON values carry.
#[derive(Clone, Copy, Default, PartialEq, PartialOrd)]
pub struct Number(f64);

impl Number {
    /// A finite number, or `None` for NaN and infinities.
    pub fn new(value: f64) -> Option<Self> {
        value.is_finite().then_some(Self(value))
    }

    pub fn as_f64(self) -> f64 {
        self.0
    }

    /// The value as an integer when it is integral and exactly representable.
    pub fn as_i64(self) -> Option<i64> {
        let value = self.0;
        (value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0).then_some(value as i64)
    }

    /// The value as a non-negative integer when it is integral and exactly representable.
    pub fn as_u64(self) -> Option<u64> {
        self.as_i64().and_then(|value| u64::try_from(value).ok())
    }

    /// Whether this is an integer (JavaScript `Number.isInteger`).
    pub fn is_integer(self) -> bool {
        self.0.fract() == 0.0
    }
}

impl fmt::Debug for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_js_number(self.0))
    }
}

macro_rules! from_integer {
    ($($ty:ty),*) => {$(
        impl From<$ty> for Number {
            fn from(value: $ty) -> Self {
                Self(value as f64)
            }
        }
    )*};
}
from_integer!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

/// Format a finite double exactly like JavaScript's `String(number)`.
pub fn format_js_number(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    if !value.is_finite() {
        return if value.is_nan() {
            "NaN".into()
        } else if value > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    let mut buffer = ryu::Buffer::new();
    let shortest = buffer.format_finite(value.abs());
    let (digits, exponent) = decimal_digits(shortest);
    let sign = if value < 0.0 { "-" } else { "" };
    let k = digits.len() as i32;
    let n = exponent;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let mantissa = if k == 1 { digits.clone() } else { format!("{}.{}", &digits[..1], &digits[1..]) };
        format!("{mantissa}e{}{}", if e >= 0 { "+" } else { "-" }, e.abs())
    };
    format!("{sign}{body}")
}

/// Split ryu output into significant digits and the decimal exponent `n` where value = 0.d1d2... * 10^n.
fn decimal_digits(text: &str) -> (String, i32) {
    let (mantissa, exp) = match text.split_once(['e', 'E']) {
        Some((mantissa, exp)) => (mantissa, exp.parse::<i32>().unwrap_or(0)),
        None => (text, 0),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let frac_part = frac_part.trim_end_matches('0');
    let mut digits = format!("{int_part}{frac_part}");
    let mut point = int_part.len() as i32;
    let leading = digits.len() - digits.trim_start_matches('0').len();
    digits.drain(..leading);
    point -= leading as i32;
    let trimmed = digits.trim_end_matches('0').len();
    digits.truncate(trimmed.max(1));
    (digits, point + exp)
}

#[cfg(test)]
mod tests {
    use super::format_js_number;

    #[test]
    fn matches_javascript_number_to_string() {
        let cases: [(f64, &str); 16] = [
            (1.0, "1"),
            (-1.5, "-1.5"),
            (0.1, "0.1"),
            (100.0, "100"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (123456789012345680000.0, "123456789012345680000"),
            (0.000001, "0.000001"),
            (1e-7, "1e-7"),
            (1.5e-7, "1.5e-7"),
            (-0.0, "0"),
            (2.5e25, "2.5e+25"),
            (0.3, "0.3"),
            (9007199254740993.0, "9007199254740992"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
        ];
        for (value, expected) in cases {
            assert_eq!(format_js_number(value), expected, "{value}");
        }
    }
}
