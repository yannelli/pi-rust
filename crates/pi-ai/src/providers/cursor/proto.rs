//! The protobuf wire format subset the Cursor agent protocol needs (varints, length-delimited
//! fields, fixed 32/64-bit skips) and Connect envelope framing.

/// Builds one protobuf message.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Writer {
    buffer: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(&mut self, field: u32, wire_type: u8) {
        self.raw_varint((u64::from(field) << 3) | u64::from(wire_type));
    }

    fn raw_varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.buffer.push((value as u8) | 0x80);
            value >>= 7;
        }
        self.buffer.push(value as u8);
    }

    pub fn varint(mut self, field: u32, value: u64) -> Self {
        self.key(field, 0);
        self.raw_varint(value);
        self
    }

    pub fn double(mut self, field: u32, value: f64) -> Self {
        self.key(field, 1);
        self.buffer.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn bool(self, field: u32, value: bool) -> Self {
        self.varint(field, u64::from(value))
    }

    pub fn bytes(mut self, field: u32, value: &[u8]) -> Self {
        self.key(field, 2);
        self.raw_varint(value.len() as u64);
        self.buffer.extend_from_slice(value);
        self
    }

    pub fn string(self, field: u32, value: &str) -> Self {
        self.bytes(field, value.as_bytes())
    }

    pub fn message(self, field: u32, value: Writer) -> Self {
        self.bytes(field, &value.buffer)
    }

    pub fn finish(self) -> Vec<u8> {
        self.buffer
    }
}

/// One decoded field value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed64(u64),
    Fixed32(u32),
}

impl<'a> Value<'a> {
    pub fn as_u64(self) -> Option<u64> {
        match self {
            Self::Varint(value) | Self::Fixed64(value) => Some(value),
            Self::Fixed32(value) => Some(u64::from(value)),
            Self::Bytes(_) => None,
        }
    }

    pub fn as_bytes(self) -> Option<&'a [u8]> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    pub fn as_str(self) -> Option<&'a str> {
        self.as_bytes().and_then(|bytes| std::str::from_utf8(bytes).ok())
    }
}

/// Reads the fields of one message in wire order.
pub(crate) struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = self.data.split_first().ok_or("truncated protobuf varint")?;
            self.data = rest;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("protobuf varint is too long".into())
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        if self.data.len() < len {
            return Err("truncated protobuf field".into());
        }
        let (head, rest) = self.data.split_at(len);
        self.data = rest;
        Ok(head)
    }

    /// The next `(field number, value)`, or `None` at the end of the message.
    pub fn next_field(&mut self) -> Result<Option<(u32, Value<'a>)>, String> {
        if self.data.is_empty() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = u32::try_from(key >> 3).map_err(|_| "protobuf field number is too large")?;
        let value = match key & 7 {
            0 => Value::Varint(self.varint()?),
            1 => Value::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| "bad fixed64")?)),
            2 => {
                let len = usize::try_from(self.varint()?).map_err(|_| "protobuf length is too large")?;
                Value::Bytes(self.take(len)?)
            }
            5 => Value::Fixed32(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| "bad fixed32")?)),
            other => return Err(format!("unsupported protobuf wire type {other}")),
        };
        Ok(Some((field, value)))
    }
}

/// Every field of `data`, for messages read by field number.
pub(crate) fn fields(data: &[u8]) -> Result<Vec<(u32, Value<'_>)>, String> {
    let mut reader = Reader::new(data);
    let mut out = Vec::new();
    while let Some(field) = reader.next_field()? {
        out.push(field);
    }
    Ok(out)
}

/// The last value of `field`, as proto3 merges repeated scalars.
pub(crate) fn field<'a>(fields: &[(u32, Value<'a>)], number: u32) -> Option<Value<'a>> {
    fields.iter().rev().find(|(field, _)| *field == number).map(|(_, value)| *value)
}

pub(crate) const FLAG_COMPRESSED: u8 = 0x01;
pub(crate) const FLAG_END_STREAM: u8 = 0x02;

/// A Connect envelope: a flag byte, a big-endian length, and the payload.
pub(crate) fn envelope(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.push(flags);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Splits Connect envelopes out of a byte stream as chunks arrive.
#[derive(Default)]
pub(crate) struct EnvelopeReader {
    pending: Vec<u8>,
}

impl EnvelopeReader {
    pub fn push(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
    }

    /// The next complete `(flags, payload)`, or `None` until more bytes arrive.
    pub fn next_envelope(&mut self) -> Option<(u8, Vec<u8>)> {
        let header: [u8; 4] = self.pending.get(1..5)?.try_into().ok()?;
        let len = u32::from_be_bytes(header) as usize;
        if self.pending.len() < len + 5 {
            return None;
        }
        let flags = self.pending[0];
        let payload = self.pending[5..len + 5].to_vec();
        self.pending.drain(..len + 5);
        Some((flags, payload))
    }

    pub fn has_partial(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_reads_nested_fields() {
        let inner = Writer::new().string(1, "hi").varint(2, 300);
        let data = Writer::new().message(3, inner).bool(4, true).finish();
        let outer = fields(&data).unwrap();
        let inner = fields(field(&outer, 3).unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(field(&inner, 1).unwrap().as_str(), Some("hi"));
        assert_eq!(field(&inner, 2).unwrap().as_u64(), Some(300));
        assert_eq!(field(&outer, 4).unwrap().as_u64(), Some(1));
    }

    #[test]
    fn envelopes_split_across_chunks() {
        let framed = [envelope(0, b"abc"), envelope(FLAG_END_STREAM, b"{}")].concat();
        let mut reader = EnvelopeReader::default();
        reader.push(&framed[..4]);
        assert_eq!(reader.next_envelope(), None);
        reader.push(&framed[4..]);
        assert_eq!(reader.next_envelope(), Some((0, b"abc".to_vec())));
        assert_eq!(reader.next_envelope(), Some((FLAG_END_STREAM, b"{}".to_vec())));
        assert!(!reader.has_partial());
    }

    #[test]
    fn truncated_input_is_an_error() {
        assert!(fields(&[0x0a, 0x05, b'a']).is_err());
    }
}
