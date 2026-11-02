//! The message V8's `JSON.parse` throws for settings text, computed from the text. It derives from
//! V8 (BSD-3-Clause, `LICENSE-v8`), as the pi-js scanner does; positions count UTF-16 code units.

struct Scan {
    units: Vec<u16>,
    at: usize,
}

const NOT_VALID: [&str; 4] = ["[object Object]", "undefined", "Infinity", "NaN"];

fn is_digit(c: i32) -> bool {
    (48..=57).contains(&c)
}

fn is_number_part(c: i32) -> bool {
    is_digit(c) || matches!(c, 46 | 101 | 69 | 45 | 43)
}

impl Scan {
    fn code(&self, at: usize) -> i32 {
        self.units.get(at).map_or(-1, |unit| i32::from(*unit))
    }

    fn text(&self, from: usize, to: usize) -> String {
        String::from_utf16_lossy(&self.units[from.min(self.units.len())..to.min(self.units.len())])
    }

    fn located(&self, text: &str, at: usize, json: bool) -> String {
        let (mut line, mut line_start, mut i) = (1, 0, 0);
        while i < at {
            if self.code(i) == 13 && i + 1 < at && self.code(i + 1) == 10 {
                i += 1;
            }
            if matches!(self.code(i), 10 | 13) {
                line += 1;
                line_start = i + 1;
            }
            i += 1;
        }
        let context = if json { " in JSON" } else { "" };
        format!("{text}{context} at position {at} (line {line} column {})", at + 1 - line_start)
    }

    fn unexpected(&self, at: usize) -> String {
        let c = self.code(at);
        let length = self.units.len();
        let source = self.text(0, length);
        if c < 0 {
            return "Unexpected end of JSON input".into();
        }
        if c == 34 {
            return self.located("Unexpected string", at, true);
        }
        if c == 45 || is_digit(c) {
            return self.located("Unexpected number", at, true);
        }
        if NOT_VALID.contains(&source.as_str()) {
            return format!("\"{source}\" is not valid JSON");
        }
        let token = format!("Unexpected token '{}', ", self.text(at, at + 1));
        if length < 21 {
            format!("{token}\"{source}\" is not valid JSON")
        } else if at < 10 {
            format!("{token}\"{}\"... is not valid JSON", self.text(0, at + 10))
        } else if at + 10 < length {
            format!("{token}...\"{}\"... is not valid JSON", self.text(at - 10, at + 10))
        } else {
            format!("{token}...\"{}\" is not valid JSON", self.text(at - 10, length))
        }
    }

    fn skip(&mut self) {
        while matches!(self.code(self.at), 32 | 9 | 13 | 10) {
            self.at += 1;
        }
    }

    fn digits(&mut self) {
        while is_digit(self.code(self.at)) {
            self.at += 1;
        }
    }

    fn string(&mut self) -> Option<String> {
        self.at += 1;
        loop {
            let c = self.code(self.at);
            if c < 0 {
                return Some(self.located("Unterminated string", self.at, true));
            }
            if c == 34 {
                self.at += 1;
                return None;
            }
            if c < 32 {
                return Some(self.located("Bad control character in string literal", self.at, true));
            }
            if c == 92 {
                self.at += 1;
                let escape = self.code(self.at);
                if !(0..=255).contains(&escape) {
                    return Some(self.unexpected(self.at));
                }
                if escape == 117 {
                    for _ in 0..4 {
                        self.at += 1;
                        let hex = u8::try_from(self.code(self.at)).is_ok_and(|unit| unit.is_ascii_hexdigit());
                        if !hex {
                            return Some(self.located("Bad Unicode escape", self.at, true));
                        }
                    }
                } else if !b"\"\\/bfnrt".contains(&(escape as u8)) {
                    return Some(self.located("Bad escaped character", self.at, true));
                }
            }
            self.at += 1;
        }
    }

    fn number(&mut self) -> Option<String> {
        let negative = self.code(self.at) == 45;
        if negative {
            self.at += 1;
        }
        if self.code(self.at) == 48 {
            self.at += 1;
            let c = self.code(self.at);
            if is_number_part(c) && is_digit(c) {
                return Some(self.unexpected(self.at));
            }
            if !is_number_part(c) && !negative {
                return None;
            }
        } else {
            let start = self.at;
            self.digits();
            if self.at == start {
                return Some(self.located("No number after minus sign", self.at, true));
            }
            if !is_number_part(self.code(self.at)) {
                return None;
            }
        }
        if self.code(self.at) == 46 {
            self.at += 1;
            if !is_digit(self.code(self.at)) {
                return Some(self.located("Unterminated fractional number", self.at, true));
            }
            self.digits();
        }
        if (self.code(self.at) | 32) == 101 {
            self.at += 1;
            if matches!(self.code(self.at), 45 | 43) {
                self.at += 1;
            }
            if !is_digit(self.code(self.at)) {
                return Some(self.located("Exponent part is missing a number", self.at, true));
            }
            self.digits();
        }
        None
    }

    fn literal(&mut self, word: &str) -> Option<String> {
        for unit in word.bytes().skip(1) {
            self.at += 1;
            if self.code(self.at) != i32::from(unit) {
                return Some(self.unexpected(self.at));
            }
        }
        self.at += 1;
        None
    }

    fn member(&mut self, missing: &str) -> Option<String> {
        self.skip();
        if self.code(self.at) != 34 {
            return Some(self.located(missing, self.at, true));
        }
        if let Some(error) = self.string() {
            return Some(error);
        }
        self.skip();
        if self.code(self.at) != 58 {
            return Some(self.located("Expected ':' after property name", self.at, true));
        }
        self.at += 1;
        None
    }

    /// The value at `at`, or `Err` with the message; `Ok(true)` opened a container.
    fn value(&mut self, open: &mut Vec<i32>) -> Result<bool, String> {
        self.skip();
        let c = self.code(self.at);
        let error = match c {
            34 => self.string(),
            45 | 48..=57 => self.number(),
            116 => self.literal("true"),
            102 => self.literal("false"),
            110 => self.literal("null"),
            123 | 91 => {
                self.at += 1;
                self.skip();
                if self.code(self.at) == c + 2 {
                    self.at += 1;
                    None
                } else if c == 91 {
                    open.push(c);
                    return Ok(true);
                } else {
                    if let Some(error) = self.member("Expected property name or '}'") {
                        return Err(error);
                    }
                    open.push(c);
                    return Ok(true);
                }
            }
            _ => return Err(self.unexpected(self.at)),
        };
        error.map_or(Ok(false), Err)
    }
}

/// V8's message for `text`, or `None` when V8 parses it.
pub(super) fn message(text: &str) -> Option<String> {
    let mut scan = Scan { units: text.encode_utf16().collect(), at: 0 };
    let mut open = Vec::new();
    loop {
        match scan.value(&mut open) {
            Err(error) => return Some(error),
            Ok(true) => continue,
            Ok(false) => {}
        }
        loop {
            scan.skip();
            let Some(&container) = open.last() else {
                let after = "Unexpected non-whitespace character after JSON";
                return (scan.at < scan.units.len()).then(|| scan.located(after, scan.at, false));
            };
            if scan.code(scan.at) == 44 {
                scan.at += 1;
                let error = (container == 123).then(|| scan.member("Expected double-quoted property name")).flatten();
                if error.is_some() {
                    return error;
                }
                break;
            }
            if scan.code(scan.at) != container + 2 {
                let after =
                    if container == 91 { "',' or ']' after array element" } else { "',' or '}' after property value" };
                return Some(scan.located(&format!("Expected {after}"), scan.at, true));
            }
            scan.at += 1;
            open.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::message;

    #[test]
    fn messages_match_the_oracle() {
        let position =
            |at: usize| format!("Expected property name or '}}' in JSON at position {at} (line 1 column {})", at + 1);
        assert_eq!(message("{ not json").as_deref(), Some(position(2).as_str()));
        assert_eq!(message("{").as_deref(), Some(position(1).as_str()));
        assert_eq!(message("[1,").as_deref(), Some("Unexpected end of JSON input"));
        assert_eq!(message("{\"a\": [1, {\"b\": true}], \"c\": null}"), None);
        assert_eq!(
            message("{} x").as_deref(),
            Some("Unexpected non-whitespace character after JSON at position 3 (line 1 column 4)")
        );
        assert_eq!(
            message("{\"a\" 1}").as_deref(),
            Some("Expected ':' after property name in JSON at position 5 (line 1 column 6)")
        );
    }
}
