//! Decoder for exactly one item of the strict subset.

use std::collections::HashSet;

use pi_chord::Json;

use super::{CborError, CborOptions, CborValue, MAX_SAFE_INTEGER, Resolved, UINT32_BASE};

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
    options: Resolved,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, CborError> {
        let value = *self.bytes.get(self.offset).ok_or_else(|| CborError::new("Truncated CBOR payload"))?;
        self.offset += 1;
        Ok(value)
    }

    fn take(&mut self, length: u64) -> Result<&'a [u8], CborError> {
        if length > (self.bytes.len() - self.offset) as u64 {
            return Err(CborError::new("Truncated CBOR payload"));
        }
        let start = self.offset;
        self.offset += length as usize;
        Ok(&self.bytes[start..self.offset])
    }

    fn argument(&mut self, info: u8) -> Result<u64, CborError> {
        if info < 24 {
            return Ok(u64::from(info));
        }
        match info {
            24 => Ok(u64::from(self.byte()?)),
            25 => {
                let bytes = self.take(2)?;
                Ok(u64::from(u16::from_be_bytes([bytes[0], bytes[1]])))
            }
            26 => {
                let bytes = self.take(4)?;
                Ok(u64::from(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])))
            }
            27 => {
                let high = self.argument(26)?;
                let low = self.argument(26)?;
                if high > 0x1f_ffff {
                    return Err(CborError::new("Decoded CBOR integer or length is outside the safe range"));
                }
                Ok(high * UINT32_BASE + low)
            }
            31 => Err(CborError::new("Indefinite-length CBOR items are not supported")),
            _ => Err(CborError::new("Malformed CBOR additional information")),
        }
    }

    fn length(&mut self, info: u8, kind: &str, limit: usize) -> Result<u64, CborError> {
        if info == 31 {
            return Err(CborError::new(format!("Indefinite-length CBOR {kind}s are not supported")));
        }
        let length = self.argument(info)?;
        if length > limit as u64 {
            return Err(CborError::new(format!("CBOR {kind} length exceeds configured limit of {limit}")));
        }
        Ok(length)
    }

    fn item(&mut self, depth: usize) -> Result<CborValue, CborError> {
        if depth > self.options.max_depth {
            return Err(CborError::new(format!(
                "CBOR nesting depth exceeds configured limit of {}",
                self.options.max_depth
            )));
        }
        let initial = self.byte()?;
        let info = initial & 0x1f;
        match initial >> 5 {
            0 => Ok(CborValue::Number(self.argument(info)? as f64)),
            1 => {
                let value = -1.0 - self.argument(info)? as f64;
                if value < -MAX_SAFE_INTEGER {
                    return Err(CborError::new("Decoded CBOR integer is outside the safe range"));
                }
                Ok(CborValue::Number(value))
            }
            2 => {
                let length = self.length(info, "byte string", self.options.max_byte_length)?;
                Ok(CborValue::Bytes(self.take(length)?.to_vec()))
            }
            3 => {
                let length = self.length(info, "text string", self.options.max_byte_length)?;
                let bytes = self.take(length)?;
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| CborError::new("CBOR text string contains invalid UTF-8"))?;
                Ok(CborValue::Text(text.to_string()))
            }
            4 => {
                let length = self.length(info, "array", self.options.max_container_length)?;
                let mut items = Vec::with_capacity(length.min(1024) as usize);
                for _ in 0..length {
                    items.push(self.item(depth + 1)?);
                }
                Ok(CborValue::Array(items))
            }
            5 => {
                let length = self.length(info, "map", self.options.max_container_length)?;
                let mut entries = Vec::with_capacity(length.min(1024) as usize);
                let mut keys = HashSet::new();
                for _ in 0..length {
                    let CborValue::Text(key) = self.item(depth + 1)? else {
                        return Err(CborError::new("CBOR map keys must be strings"));
                    };
                    if !keys.insert(key.clone()) {
                        return Err(CborError::new("CBOR map contains a duplicate key"));
                    }
                    let value = self.item(depth + 1)?;
                    entries.push((key, value));
                }
                Ok(CborValue::Map(entries))
            }
            6 => Err(CborError::new("CBOR tags are not supported")),
            _ => self.simple(info),
        }
    }

    fn simple(&mut self, info: u8) -> Result<CborValue, CborError> {
        match info {
            20 => Ok(CborValue::Bool(false)),
            21 => Ok(CborValue::Bool(true)),
            22 => Ok(CborValue::Null),
            27 => {
                let bytes = self.take(8)?;
                let value = f64::from_be_bytes(bytes.try_into().unwrap_or([0; 8]));
                if !value.is_finite() {
                    return Err(CborError::new("Decoded CBOR number must be finite"));
                }
                if value.fract() == 0.0 && value.abs() > MAX_SAFE_INTEGER {
                    return Err(CborError::new("Decoded CBOR integer is outside the safe range"));
                }
                Ok(CborValue::Number(value))
            }
            31 => Err(CborError::new("CBOR break marker is not supported")),
            _ => Err(CborError::new("Unsupported CBOR simple value or floating-point width")),
        }
    }
}

/// Decode exactly one item from the protocol's strict RFC 8949 subset.
pub fn decode_cbor(bytes: &[u8], options: &CborOptions) -> Result<CborValue, CborError> {
    let options = options.resolve()?;
    if bytes.len() > options.max_byte_length {
        return Err(CborError::new(format!(
            "CBOR byte length exceeds configured limit of {}",
            options.max_byte_length
        )));
    }
    let mut reader = Reader { bytes, offset: 0, options };
    let value = reader.item(0)?;
    if reader.offset != bytes.len() {
        return Err(CborError::new("CBOR payload contains trailing data"));
    }
    Ok(value)
}

/// Decode one item and require strict JSON, rejecting byte strings.
pub fn decode_json_cbor(bytes: &[u8], options: &CborOptions) -> Result<Json, CborError> {
    decode_cbor(bytes, options)?.to_json().ok_or_else(|| CborError::new("CBOR value is not strict JSON"))
}
