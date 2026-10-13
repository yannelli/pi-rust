//! Encoder for the strict, definite-length subset.

use std::collections::HashSet;

use pi_chord::Json;

use super::{CborError, CborOptions, CborValue, MAX_SAFE_INTEGER, Resolved, UINT32_BASE};

/// Borrowed view of one item, shared by [`CborValue`] and [`Json`] inputs.
enum View<'a, T> {
    Null,
    Bool(bool),
    Number(f64),
    Bytes(&'a [u8]),
    Text(&'a str),
    Array(&'a [T]),
    Map(Vec<(&'a str, &'a T)>),
}

trait Item: Sized {
    fn view(&self) -> View<'_, Self>;
}

impl Item for CborValue {
    fn view(&self) -> View<'_, Self> {
        match self {
            CborValue::Null => View::Null,
            CborValue::Bool(value) => View::Bool(*value),
            CborValue::Number(value) => View::Number(*value),
            CborValue::Bytes(bytes) => View::Bytes(bytes),
            CborValue::Text(text) => View::Text(text),
            CborValue::Array(items) => View::Array(items),
            CborValue::Map(entries) => View::Map(entries.iter().map(|(key, value)| (key.as_str(), value)).collect()),
        }
    }
}

impl Item for Json {
    fn view(&self) -> View<'_, Self> {
        match self {
            Json::Null => View::Null,
            Json::Bool(value) => View::Bool(*value),
            Json::Number(number) => View::Number(number.as_f64()),
            Json::String(text) => View::Text(text),
            Json::Array(items) => View::Array(items),
            Json::Object(map) => View::Map(map.iter().map(|(key, value)| (&**key, value)).collect()),
        }
    }
}

struct Writer {
    buffer: Vec<u8>,
    max_byte_length: usize,
}

impl Writer {
    fn ensure(&self, additional: usize) -> Result<(), CborError> {
        if self.buffer.len() + additional > self.max_byte_length {
            return Err(CborError::new(format!(
                "CBOR byte length exceeds configured limit of {}",
                self.max_byte_length
            )));
        }
        Ok(())
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<(), CborError> {
        self.ensure(bytes.len())?;
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    fn argument(&mut self, major: u8, value: u64) -> Result<(), CborError> {
        let prefix = major << 5;
        if value < 24 {
            self.bytes(&[prefix | value as u8])
        } else if value <= 0xff {
            self.bytes(&[prefix | 24, value as u8])
        } else if value <= 0xffff {
            self.bytes(&[prefix | 25])?;
            self.bytes(&(value as u16).to_be_bytes())
        } else if value < UINT32_BASE {
            self.bytes(&[prefix | 26])?;
            self.bytes(&(value as u32).to_be_bytes())
        } else {
            self.bytes(&[prefix | 27])?;
            self.bytes(&value.to_be_bytes())
        }
    }

    fn text(&mut self, value: &str, options: &Resolved) -> Result<(), CborError> {
        if value.len() > options.max_byte_length {
            return Err(CborError::new(format!(
                "CBOR text string length exceeds configured limit of {}",
                options.max_byte_length
            )));
        }
        self.argument(3, value.len() as u64)?;
        self.bytes(value.as_bytes())
    }
}

fn encode_number(writer: &mut Writer, value: f64) -> Result<(), CborError> {
    if !value.is_finite() {
        return Err(CborError::new("CBOR numbers must be finite"));
    }
    let negative_zero = value == 0.0 && value.is_sign_negative();
    if value.fract() == 0.0 && !negative_zero {
        if value.abs() > MAX_SAFE_INTEGER {
            return Err(CborError::new("CBOR integers must be safe JavaScript integers"));
        }
        return if value >= 0.0 { writer.argument(0, value as u64) } else { writer.argument(1, (-1.0 - value) as u64) };
    }
    writer.bytes(&[0xfb])?;
    writer.bytes(&value.to_be_bytes())
}

fn encode_item<T: Item>(writer: &mut Writer, value: &T, options: &Resolved, depth: usize) -> Result<(), CborError> {
    if depth > options.max_depth {
        return Err(CborError::new(format!("CBOR nesting depth exceeds configured limit of {}", options.max_depth)));
    }
    match value.view() {
        View::Null => writer.bytes(&[0xf6]),
        View::Bool(value) => writer.bytes(&[if value { 0xf5 } else { 0xf4 }]),
        View::Number(value) => encode_number(writer, value),
        View::Text(text) => writer.text(text, options),
        View::Bytes(bytes) => {
            if bytes.len() > options.max_byte_length {
                return Err(CborError::new(format!(
                    "CBOR byte string length exceeds configured limit of {}",
                    options.max_byte_length
                )));
            }
            writer.argument(2, bytes.len() as u64)?;
            writer.bytes(bytes)
        }
        View::Array(items) => {
            if items.len() > options.max_container_length {
                return Err(CborError::new(format!(
                    "CBOR array length exceeds configured limit of {}",
                    options.max_container_length
                )));
            }
            writer.argument(4, items.len() as u64)?;
            items.iter().try_for_each(|item| encode_item(writer, item, options, depth + 1))
        }
        View::Map(entries) => {
            if entries.len() > options.max_container_length {
                return Err(CborError::new(format!(
                    "CBOR map length exceeds configured limit of {}",
                    options.max_container_length
                )));
            }
            let mut keys = HashSet::with_capacity(entries.len());
            if !entries.iter().all(|(key, _)| keys.insert(*key)) {
                return Err(CborError::new("CBOR map contains a duplicate key"));
            }
            writer.argument(5, entries.len() as u64)?;
            for (key, entry) in entries {
                writer.text(key, options)?;
                encode_item(writer, entry, options, depth + 1)?;
            }
            Ok(())
        }
    }
}

fn encode<T: Item>(value: &T, options: &CborOptions) -> Result<Vec<u8>, CborError> {
    let resolved = options.resolve()?;
    let mut writer = Writer {
        buffer: Vec::with_capacity(resolved.max_byte_length.min(256)),
        max_byte_length: resolved.max_byte_length,
    };
    encode_item(&mut writer, value, &resolved, 0)?;
    Ok(writer.buffer)
}

/// Encode one value in the protocol's strict, definite-length RFC 8949 subset.
pub fn encode_cbor(value: &CborValue, options: &CborOptions) -> Result<Vec<u8>, CborError> {
    encode(value, options)
}

/// Encode a strict JSON value without an intermediate copy.
pub fn encode_json_cbor(value: &Json, options: &CborOptions) -> Result<Vec<u8>, CborError> {
    encode(value, options)
}
