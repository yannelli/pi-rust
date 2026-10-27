//! Differential fuzzing of the streaming parser against an independent reference reader, today's
//! `parse_streaming_json`, serde_json, and the JSONTestSuite corpus (upstream `streaming-json.test.ts`).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::StreamingJsonParser;
use super::tests::{final_snapshot, pieces};
use crate::json::parse_streaming_json;

/// Seeded random numbers in `[0, 1)`, so every run checks the same documents.
struct Random(u64);

impl Random {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, count: usize) -> usize {
        (self.next() * count as f64) as usize
    }

    fn pick<T: Clone>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())].clone()
    }
}

const STRING_PIECES: [&str; 17] = [
    "a",
    "b",
    "x y",
    " ",
    "\n",
    "\t",
    "\\",
    "\"",
    "/",
    "\u{e9}",
    "\u{1f600}",
    "\u{1}",
    "\u{1f}",
    "\u{2028}",
    "{",
    "]",
    ",",
];

fn random_string(random: &mut Random) -> String {
    (0..random.below(6)).map(|_| random.pick(&STRING_PIECES)).collect()
}

fn random_scalar(random: &mut Random) -> Value {
    match random.below(10) {
        0 => json!(random_string(random)),
        1 => json!(random.below(2000) as i64 - 1000),
        2 => json!(random.next() * 100.0),
        3 => json!(1e21),
        4 => json!(-5e-8),
        5 => json!(-0.0),
        6 => json!(0),
        7 => json!(true),
        8 => json!(false),
        _ => Value::Null,
    }
}

fn random_value(random: &mut Random, depth: usize) -> Value {
    let roll = random.next();
    if depth > 3 || roll < 0.3 {
        return random_scalar(random);
    }
    let count = random.below(4);
    if roll < 0.65 {
        let members: Map<String, Value> =
            (0..count).map(|_| (random_string(random), random_value(random, depth + 1))).collect();
        return Value::Object(members);
    }
    Value::Array((0..count).map(|_| random_value(random, depth + 1)).collect())
}

fn space(random: &mut Random, spaced: bool) -> &'static str {
    if spaced { random.pick(&["", "", " ", "\n  ", "\t", "\r\n"]) } else { "" }
}

/// JSON text of `value`, with random insignificant whitespace when `spaced`.
fn render(value: &Value, random: &mut Random, spaced: bool) -> String {
    let items: Vec<String> = match value {
        Value::Array(items) => items.iter().map(|item| render(item, random, spaced)).collect(),
        Value::Object(members) => members
            .iter()
            .map(|(key, item)| {
                let colon = format!("{}:{}", space(random, spaced), space(random, spaced));
                format!("{}{colon}{}", json!(key), render(item, random, spaced))
            })
            .collect(),
        scalar => return scalar.to_string(),
    };
    let (open, close) = if value.is_array() { ("[", "]") } else { ("{", "}") };
    let separator = format!("{},{}", space(random, spaced), space(random, spaced));
    format!("{open}{}{}{}{close}", space(random, spaced), items.join(&separator), space(random, spaced))
}

fn random_documents(seed: u64, count: usize, spaced: bool) -> Vec<String> {
    let mut random = Random(seed);
    (0..count)
        .map(|_| {
            let value = if random.next() < 0.8 {
                json!({"path": random_string(&mut random), "args": random_value(&mut random, 0)})
            } else {
                random_value(&mut random, 0)
            };
            render(&value, &mut random, spaced)
        })
        .collect()
}

/// Reference for the parser on a prefix of valid JSON: a recursive-descent reading of the whole
/// prefix by the documented rules, written independently so the two check each other. Unterminated
/// numbers and literals show only when `pending_scalars` is set, as snapshots show them.
pub(super) fn reference_snapshot(text: &str, pending_scalars: bool) -> Option<Value> {
    let chars: Vec<char> = text.chars().collect();
    let reader = Reader { chars: &chars, pending_scalars };
    reader.value(reader.skip_space(0)).map(|read| read.value)
}

struct Read {
    value: Value,
    complete: bool,
    end: usize,
}

struct Reader<'a> {
    chars: &'a [char],
    pending_scalars: bool,
}

impl Reader<'_> {
    fn skip_space(&self, mut index: usize) -> usize {
        while self.chars.get(index).is_some_and(|char| " \t\n\r".contains(*char)) {
            index += 1;
        }
        index
    }

    fn value(&self, start: usize) -> Option<Read> {
        match self.chars.get(start)? {
            '"' => Some(self.string(start)),
            '{' => Some(self.container(start, true)),
            '[' => Some(self.container(start, false)),
            _ => self.scalar(start),
        }
    }

    fn string(&self, start: usize) -> Read {
        let (chars, mut value, mut index) = (self.chars, String::new(), start + 1);
        let partial = |value: String| Read { value: Value::String(value), complete: false, end: chars.len() };
        while let Some(&char) = chars.get(index) {
            if char == '"' {
                return Read { value: Value::String(value), complete: true, end: index + 1 };
            }
            index += 1;
            if char != '\\' {
                value.push(char);
                continue;
            }
            let Some(&next) = chars.get(index) else { return partial(value) };
            let simple = match next {
                '"' | '\\' | '/' => Some(next),
                'b' => Some('\u{8}'),
                'f' => Some('\u{c}'),
                'n' => Some('\n'),
                'r' => Some('\r'),
                't' => Some('\t'),
                _ => None,
            };
            if let Some(decoded) = simple {
                value.push(decoded);
                index += 1;
            } else if next == 'u' {
                let digits = chars[index + 1..].iter().take(4).take_while(|char| char.is_ascii_hexdigit()).count();
                let end = index + 1 + digits;
                if digits == 4 {
                    let code: String = chars[index + 1..end].iter().collect();
                    value.extend(char::from_u32(u32::from_str_radix(&code, 16).unwrap()));
                } else if end == chars.len() {
                    return partial(value);
                } else {
                    // Not four hex digits: the escape stays literal and the next character reads normally.
                    value.extend(&chars[index - 1..end]);
                }
                index = end;
            } else {
                // Invalid escape: the backslash stays literal and the next character reads normally.
                value.push('\\');
            }
        }
        partial(value)
    }

    fn container(&self, start: usize, is_object: bool) -> Read {
        let (mut members, mut items) = (Map::new(), Vec::new());
        let close = if is_object { '}' } else { ']' };
        let mut index = self.skip_space(start + 1);
        let done = |members: Map<String, Value>, items: Vec<Value>, complete: bool, end: usize| {
            let value = if is_object { Value::Object(members) } else { Value::Array(items) };
            Read { value, complete, end }
        };
        loop {
            if index >= self.chars.len() {
                return done(members, items, false, self.chars.len());
            }
            if self.chars[index] == close {
                return done(members, items, true, index + 1);
            }
            let mut key = None;
            if is_object {
                assert_eq!(self.chars[index], '"', "the reference reads valid JSON only");
                let read = self.string(index);
                if !read.complete {
                    return done(members, items, false, self.chars.len());
                }
                index = self.skip_space(read.end);
                if index >= self.chars.len() {
                    return done(members, items, false, self.chars.len());
                }
                index = self.skip_space(index + 1);
                key = read.value.as_str().map(str::to_string);
            }
            let Some(read) = self.value(index) else { return done(members, items, false, self.chars.len()) };
            match key {
                Some(key) => drop(members.insert(key, read.value)),
                None => items.push(read.value),
            }
            if !read.complete {
                return done(members, items, false, self.chars.len());
            }
            index = self.skip_space(read.end);
            if index < self.chars.len() && self.chars[index] != close {
                index = self.skip_space(index + 1);
            }
        }
    }

    fn scalar(&self, start: usize) -> Option<Read> {
        let end = (start..self.chars.len()).find(|index| " \t\n\r,:{}[]\"".contains(self.chars[*index]));
        let token: String = self.chars[start..end.unwrap_or(self.chars.len())].iter().collect();
        if let Some(end) = end {
            return Some(Read { value: serde_json::from_str(&token).unwrap(), complete: true, end });
        }
        let pending = |value: Value| Some(Read { value, complete: false, end: self.chars.len() });
        if !self.pending_scalars {
            return None;
        }
        for (literal, value) in [("true", json!(true)), ("false", json!(false)), ("null", Value::Null)] {
            if literal.starts_with(&token) {
                return pending(value);
            }
        }
        if let Ok(value @ Value::Number(_)) = serde_json::from_str(&token) {
            return pending(value);
        }
        // A number whose exponent has no digits yet shows its mantissa.
        let mantissa = token.strip_suffix(['+', '-']).unwrap_or(&token).strip_suffix(['e', 'E'])?;
        match serde_json::from_str(mantissa) {
            Ok(value @ Value::Number(_)) if !mantissa.contains(['e', 'E']) => pending(value),
            _ => None,
        }
    }
}

/// Every prefix the parser reads `text` through, in pieces of `size` characters.
fn stream(text: &str, size: usize) -> Vec<(String, Option<Value>)> {
    let mut parser = StreamingJsonParser::new();
    let mut read = String::new();
    pieces(text, size)
        .into_iter()
        .map(|piece| {
            parser.append(piece);
            read.push_str(piece);
            (read.clone(), parser.snapshot().cloned())
        })
        .collect()
}

/// A copy kept by applying the parser's changes, checked against the reference after each piece.
fn mirror_stream(text: &str, size: usize) -> Option<Value> {
    let mut parser = StreamingJsonParser::new();
    let (mut mirror, mut read) = (None, String::new());
    for piece in pieces(text, size) {
        parser.append(piece).iter().for_each(|change| change.apply(&mut mirror));
        read.push_str(piece);
        // Changes leave out unterminated numbers and literals, which snapshots show.
        assert_eq!(mirror, reference_snapshot(&read, false), "{read:?}");
    }
    mirror
}

fn chunk_sizes(text: &str) -> [usize; 5] {
    [1, 2, 3, 7, 1 + text.len() % 12]
}

#[test]
fn matches_the_reference_after_every_piece_at_every_chunk_size() {
    for text in random_documents(1, 250, true) {
        for size in chunk_sizes(&text) {
            for (read, snapshot) in stream(&text, size) {
                assert_eq!(snapshot, reference_snapshot(&read, true), "{read:?} in pieces of {size}");
            }
        }
    }
}

#[test]
fn matches_parse_streaming_json_at_every_prefix_of_compact_documents() {
    for text in random_documents(2, 400, false) {
        for (read, snapshot) in stream(&text, 1) {
            // Documented differences: parse_streaming_json trims trailing whitespace, drops unterminated
            // literals, cuts numbers back, and shows an unterminated `\u` escape as text.
            let open_escape = read.rsplit_once("\\u").is_some_and(|(_, tail)| tail.len() < 4);
            let trimmed = read.trim_end_matches('\\').ends_with(char::is_whitespace);
            let open_scalar = reference_snapshot(&read, true) != reference_snapshot(&read, false)
                || read.ends_with(['.', 'e', 'E', '+', '-']);
            if trimmed || open_scalar || open_escape {
                continue;
            }
            assert_eq!(snapshot, parse_streaming_json(&read), "{read:?}");
        }
    }
}

#[test]
fn reads_complete_documents_exactly_like_serde_json() {
    for text in random_documents(3, 300, true).into_iter().chain(random_documents(4, 300, false)) {
        let expected: Value = serde_json::from_str(&text).unwrap();
        for size in [text.len(), 1, 2, 3, 7, 5] {
            assert_eq!(final_snapshot(&text, size).as_ref(), Some(&expected), "{text}");
        }
    }
}

#[test]
fn repairs_raw_control_characters_and_invalid_escapes_and_shows_them_while_streaming() {
    let mut random = Random(6);
    let malformed =
        ["\n", "\t", "\u{0}", "\\d", "\\x41", "\\u12", "\\uZZZZ", "\\u00e9", "\\\\", "\\q", "a", " ", "\u{e9}"];
    for round in 0..300 {
        let content: String = (0..6).map(|_| random.pick(&malformed)).collect();
        let text = format!("{{\"path\":\"p\",\"content\":\"{content}\",\"n\":{round}}}");
        assert_eq!(final_snapshot(&text, 1), parse_streaming_json(&text), "{text:?}");
        for (read, snapshot) in stream(&text, 1) {
            assert_eq!(snapshot, reference_snapshot(&read, true), "{read:?}");
        }
    }
}

#[test]
fn changes_keep_a_copy_equal_to_the_value_read_so_far() {
    for text in random_documents(9, 200, true) {
        let expected: Value = serde_json::from_str(&text).unwrap();
        for size in chunk_sizes(&text) {
            // A trailing space ends a root number or literal, which changes report only once it ends.
            assert_eq!(mirror_stream(&format!("{text} "), size), Some(expected.clone()), "{text}");
        }
    }
}

#[test]
fn survives_random_corruption_of_valid_documents() {
    let mut random = Random(7);
    let noise = ["\"", "\\", "{", "}", "[", "]", ",", ":", "x", "1", "-", "\n", "t"];
    for text in random_documents(8, 300, true) {
        let mut characters: Vec<String> = text.chars().map(String::from).collect();
        for _ in 0..3 {
            let at = random.below(characters.len() + 1);
            if random.next() < 0.5 && at < characters.len() {
                characters.remove(at);
            } else {
                characters.insert(at, random.pick(&noise).to_string());
            }
        }
        let corrupted = characters.concat();
        let expected = final_snapshot(&corrupted, corrupted.len());
        assert_eq!(final_snapshot(&corrupted, 1 + corrupted.len() % 12), expected, "{corrupted:?}");
        // The changes keep a copy equal to the parser's own value, malformed or not.
        let (mut parser, mut mirror) = (StreamingJsonParser::new(), None);
        for piece in pieces(&corrupted, 4).into_iter().chain([" "]) {
            parser.append(piece).iter().for_each(|change| change.apply(&mut mirror));
        }
        assert_eq!(mirror.as_ref(), parser.snapshot(), "{corrupted:?}");
    }
}

#[test]
fn reads_every_json_test_suite_document_serde_json_accepts_exactly_like_it() {
    let suite: Value = serde_json::from_str(include_str!("../../tests/data/json-test-suite.json")).unwrap();
    for (name, encoded) in suite["files"].as_object().unwrap() {
        let bytes = STANDARD.decode(encoded.as_str().unwrap()).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        let expected = serde_json::from_str::<Value>(&text).ok();
        for size in [text.chars().count().max(1), 1, 3] {
            let snapshot = final_snapshot(&text, size);
            if expected.is_some() {
                assert_eq!(snapshot, expected, "{name}");
            }
        }
    }
}
