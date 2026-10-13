//! AWS event stream binary framing (`application/vnd.amazon.eventstream`). A message is the
//! total and headers lengths (u32 each), a prelude CRC32, the headers, the payload, and a
//! CRC32 of everything before it. All integers are big-endian.

const PRELUDE_LENGTH: usize = 12;
const MIN_MESSAGE_LENGTH: usize = PRELUDE_LENGTH + 4;
/// Largest accepted message, matching the AWS SDK limit.
const MAX_MESSAGE_LENGTH: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderValue {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Bytes(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid([u8; 16]),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub headers: Vec<(String, HeaderValue)>,
    pub payload: Vec<u8>,
}

impl Message {
    /// A string header value, such as `:event-type`.
    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.headers.iter().find_map(|(key, value)| match value {
            HeaderValue::String(text) if key == name => Some(text.as_str()),
            _ => None,
        })
    }
}

/// Incremental decoder: push bytes as they arrive, then drain complete messages.
#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// True when buffered bytes remain that do not form a complete message.
    pub fn has_partial(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// The next complete message, `None` when more bytes are needed.
    pub fn next_message(&mut self) -> Result<Option<Message>, String> {
        if self.buffer.len() < PRELUDE_LENGTH {
            return Ok(None);
        }
        let total = read_u32(&self.buffer, 0) as usize;
        let headers_length = read_u32(&self.buffer, 4) as usize;
        let prelude_crc = read_u32(&self.buffer, 8);
        if crc32fast::hash(&self.buffer[..8]) != prelude_crc {
            return Err("Event stream prelude checksum mismatch".into());
        }
        if !(MIN_MESSAGE_LENGTH..=MAX_MESSAGE_LENGTH).contains(&total) || headers_length > total - MIN_MESSAGE_LENGTH {
            return Err(format!("Invalid event stream message length: {total} (headers {headers_length})"));
        }
        if self.buffer.len() < total {
            return Ok(None);
        }
        let frame: Vec<u8> = self.buffer.drain(..total).collect();
        let message_crc = read_u32(&frame, total - 4);
        if crc32fast::hash(&frame[..total - 4]) != message_crc {
            return Err("Event stream message checksum mismatch".into());
        }
        let headers = parse_headers(&frame[PRELUDE_LENGTH..PRELUDE_LENGTH + headers_length])?;
        let payload = frame[PRELUDE_LENGTH + headers_length..total - 4].to_vec();
        Ok(Some(Message { headers, payload }))
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]])
}

fn take<'a>(bytes: &'a [u8], offset: &mut usize, length: usize) -> Result<&'a [u8], String> {
    let slice = bytes.get(*offset..*offset + length).ok_or("Truncated event stream header")?;
    *offset += length;
    Ok(slice)
}

fn fixed<const N: usize>(bytes: &[u8], offset: &mut usize) -> Result<[u8; N], String> {
    let mut array = [0u8; N];
    array.copy_from_slice(take(bytes, offset, N)?);
    Ok(array)
}

fn parse_headers(bytes: &[u8]) -> Result<Vec<(String, HeaderValue)>, String> {
    let mut headers = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let name_length = take(bytes, &mut offset, 1)?[0] as usize;
        let name = String::from_utf8_lossy(take(bytes, &mut offset, name_length)?).into_owned();
        let value = match take(bytes, &mut offset, 1)?[0] {
            0 => HeaderValue::Bool(true),
            1 => HeaderValue::Bool(false),
            2 => HeaderValue::Byte(i8::from_be_bytes(fixed(bytes, &mut offset)?)),
            3 => HeaderValue::Short(i16::from_be_bytes(fixed(bytes, &mut offset)?)),
            4 => HeaderValue::Int(i32::from_be_bytes(fixed(bytes, &mut offset)?)),
            5 => HeaderValue::Long(i64::from_be_bytes(fixed(bytes, &mut offset)?)),
            kind @ (6 | 7) => {
                let length = u16::from_be_bytes(fixed(bytes, &mut offset)?) as usize;
                let data = take(bytes, &mut offset, length)?;
                if kind == 6 {
                    HeaderValue::Bytes(data.to_vec())
                } else {
                    HeaderValue::String(String::from_utf8_lossy(data).into_owned())
                }
            }
            8 => HeaderValue::Timestamp(i64::from_be_bytes(fixed(bytes, &mut offset)?)),
            9 => HeaderValue::Uuid(fixed(bytes, &mut offset)?),
            kind => return Err(format!("Unknown event stream header type: {kind}")),
        };
        headers.push((name, value));
    }
    Ok(headers)
}

/// Encode a message; used by tests and mock servers.
#[cfg(test)]
pub fn encode(message: &Message) -> Vec<u8> {
    let mut headers = Vec::new();
    for (name, value) in &message.headers {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        let (kind, data): (u8, Vec<u8>) = match value {
            HeaderValue::Bool(true) => (0, Vec::new()),
            HeaderValue::Bool(false) => (1, Vec::new()),
            HeaderValue::Byte(byte) => (2, byte.to_be_bytes().to_vec()),
            HeaderValue::Short(short) => (3, short.to_be_bytes().to_vec()),
            HeaderValue::Int(int) => (4, int.to_be_bytes().to_vec()),
            HeaderValue::Long(long) => (5, long.to_be_bytes().to_vec()),
            HeaderValue::Bytes(data) => (6, [&(data.len() as u16).to_be_bytes()[..], data].concat()),
            HeaderValue::String(text) => (7, [&(text.len() as u16).to_be_bytes()[..], text.as_bytes()].concat()),
            HeaderValue::Timestamp(time) => (8, time.to_be_bytes().to_vec()),
            HeaderValue::Uuid(uuid) => (9, uuid.to_vec()),
        };
        headers.push(kind);
        headers.extend(data);
    }
    let total = (MIN_MESSAGE_LENGTH + headers.len() + message.payload.len()) as u32;
    let mut frame = Vec::with_capacity(total as usize);
    frame.extend_from_slice(&total.to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&message.payload);
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_empty_message_vector() {
        let mut decoder = Decoder::default();
        decoder.push(&[0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x05, 0xc2, 0x48, 0xeb, 0x7d, 0x98, 0xc8, 0xff]);
        assert_eq!(decoder.next_message(), Ok(Some(Message::default())));
        assert_eq!(decoder.next_message(), Ok(None));
    }

    #[test]
    fn round_trips_every_header_type_across_split_chunks() {
        let message = Message {
            headers: vec![
                (":message-type".into(), HeaderValue::String("event".into())),
                ("t".into(), HeaderValue::Bool(true)),
                ("f".into(), HeaderValue::Bool(false)),
                ("b".into(), HeaderValue::Byte(-3)),
                ("s".into(), HeaderValue::Short(-300)),
                ("i".into(), HeaderValue::Int(70_000)),
                ("l".into(), HeaderValue::Long(-5_000_000_000)),
                ("bytes".into(), HeaderValue::Bytes(vec![1, 2, 3])),
                ("time".into(), HeaderValue::Timestamp(1_700_000_000_000)),
                ("id".into(), HeaderValue::Uuid([7; 16])),
            ],
            payload: br#"{"text":"hi"}"#.to_vec(),
        };
        let mut bytes = encode(&message);
        bytes.extend(encode(&Message::default()));
        let mut decoder = Decoder::default();
        let mut decoded = Vec::new();
        for chunk in bytes.chunks(5) {
            decoder.push(chunk);
            while let Some(message) = decoder.next_message().unwrap() {
                decoded.push(message);
            }
        }
        assert_eq!(decoded, vec![message, Message::default()]);
        assert_eq!(decoded[0].header_str(":message-type"), Some("event"));
        assert!(!decoder.has_partial());
    }

    #[test]
    fn rejects_corrupted_checksums() {
        let mut frame = encode(&Message { headers: Vec::new(), payload: b"abc".to_vec() });
        let last = frame.len() - 5;
        frame[last] ^= 0xff;
        let mut decoder = Decoder::default();
        decoder.push(&frame);
        assert_eq!(decoder.next_message(), Err("Event stream message checksum mismatch".into()));
        let mut prelude = encode(&Message::default());
        prelude[3] ^= 0x01;
        let mut decoder = Decoder::default();
        decoder.push(&prelude);
        assert_eq!(decoder.next_message(), Err("Event stream prelude checksum mismatch".into()));
    }
}
