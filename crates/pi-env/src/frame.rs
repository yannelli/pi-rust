//! Frames of the wire protocol (docs/protocol.md): a big-endian header, a JSON object, and a raw payload.

use serde_json::{Map, Value};

pub(crate) const REQUEST: u8 = 1;
pub(crate) const RESULT: u8 = 2;
pub(crate) const ERROR: u8 = 3;
pub(crate) const EVENT: u8 = 4;
pub(crate) const CANCEL: u8 = 5;
pub(crate) const PING: u8 = 6;

const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Output before the sync line that is kept while searching for it.
const MAX_NOISE: usize = 1024 * 1024;

/// A JSON object of a frame.
pub type Json = Map<String, Value>;

pub(crate) fn frame(kind: u8, id: u32, json: &Json, payload: &[u8]) -> Vec<u8> {
    let body = serde_json::to_vec(json).unwrap_or_else(|_| b"{}".to_vec());
    let mut bytes = Vec::with_capacity(13 + body.len() + payload.len());
    bytes.extend_from_slice(&((9 + body.len() + payload.len()) as u32).to_be_bytes());
    bytes.push(kind);
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(payload);
    bytes
}

/// One frame received from the daemon.
pub(crate) struct Incoming {
    pub kind: u8,
    pub id: u32,
    pub json: Json,
    pub payload: Vec<u8>,
}

/// The stream is not a valid frame sequence; the connection ends.
pub(crate) struct Corrupt;

/// Splits the daemon's stdout into frames, after skipping everything before the sync line.
pub(crate) struct FrameReader {
    buffer: Vec<u8>,
    start: usize,
    marker: Option<Vec<u8>>,
}

impl FrameReader {
    pub fn new(token: &str) -> Self {
        Self { buffer: Vec::new(), start: 0, marker: Some(format!("PI-ENV {token}\n").into_bytes()) }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        if self.start > 0 && self.start * 2 >= self.buffer.len() {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        self.buffer.extend_from_slice(chunk);
    }

    /// Before the sync line: whether it arrived, and the text before it (shell startup output) to log.
    pub fn sync(&mut self) -> (bool, Option<String>) {
        let Some(marker) = &self.marker else { return (true, None) };
        let data = &self.buffer[self.start..];
        let Some(index) = data.windows(marker.len()).position(|window| window == marker.as_slice()) else {
            if data.len() > MAX_NOISE {
                self.start = self.buffer.len() - marker.len();
            }
            return (false, None);
        };
        let noise = String::from_utf8_lossy(&data[..index]).trim().to_string();
        self.start += index + marker.len();
        self.marker = None;
        (true, (!noise.is_empty()).then_some(noise))
    }

    /// The next complete frame, if one is buffered.
    pub fn next(&mut self) -> Result<Option<Incoming>, Corrupt> {
        let data = &self.buffer[self.start..];
        if data.len() < 4 {
            return Ok(None);
        }
        let length = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if !(9..=MAX_FRAME).contains(&length) {
            return Err(Corrupt);
        }
        if data.len() < 4 + length {
            return Ok(None);
        }
        let body = &data[4..4 + length];
        let kind = body[0];
        let id = u32::from_be_bytes([body[1], body[2], body[3], body[4]]);
        let json_length = u32::from_be_bytes([body[5], body[6], body[7], body[8]]) as usize;
        if 9 + json_length > length {
            return Err(Corrupt);
        }
        let Ok(Value::Object(json)) = serde_json::from_slice(&body[9..9 + json_length]) else {
            return Err(Corrupt);
        };
        let payload = body[9 + json_length..].to_vec();
        self.start += 4 + length;
        Ok(Some(Incoming { kind, id, json, payload }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_noise_before_the_sync_line_and_splits_frames_across_chunks() {
        let mut reader = FrameReader::new("ab");
        let mut json = Json::new();
        json.insert("x".into(), 1.into());
        let mut bytes = b"motd\nPI-ENV ab\n".to_vec();
        bytes.extend(frame(RESULT, 7, &json, b"pay"));
        reader.push(&bytes[..20]);
        assert_eq!(reader.sync(), (true, Some("motd".into())));
        assert!(reader.next().ok().unwrap().is_none());
        reader.push(&bytes[20..]);
        let incoming = reader.next().ok().unwrap().unwrap();
        assert_eq!((incoming.kind, incoming.id, incoming.payload.as_slice()), (RESULT, 7, &b"pay"[..]));
        assert_eq!(incoming.json, json);
    }
}
