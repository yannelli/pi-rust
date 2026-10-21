//! Payload packing, a transport optimization invisible above the links. A payload (call args, a result value, a
//! state value) whose JSON exceeds [`PACK_THRESHOLD`] UTF-16 units travels deflated; a relay forwards the bytes
//! unread ([`Packed`]) and the host that uses the payload inflates it ([`unpack`]).

use std::io::{Read, Write};
use std::sync::Arc;

use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use base64::engine::{DecodePaddingMode, Engine};
use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;

use super::Received;
use crate::error::{ChordError, ChordResult};
use crate::host2::wire::{Field, Frame, Payload, PayloadField};
use crate::json::{Json, JsonMap, utf16_len};

/// Payloads whose JSON is longer than this many UTF-16 code units are deflated.
pub const PACK_THRESHOLD: usize = 64 * 1024;

/// A deflated (raw deflate) JSON payload, kept as received until a host uses it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packed(Arc<[u8]>);

impl Packed {
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self(bytes.into())
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    /// Deflates JSON text at level 1, as upstream does.
    pub fn deflate(json: &str) -> Self {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(1));
        let _ = encoder.write_all(json.as_bytes());
        Self::new(encoder.finish().unwrap_or_default())
    }

    /// The JSON text of the payload.
    pub fn inflate(&self) -> ChordResult<String> {
        let mut text = String::new();
        DeflateDecoder::new(self.bytes())
            .read_to_string(&mut text)
            .map_err(|error| ChordError::error(format!("packed payload: {error}")))?;
        Ok(text)
    }
}

/// A frame payload as its user needs it: inflated and parsed if it arrived packed.
pub fn unpack(payload: &Payload) -> ChordResult<Json> {
    match payload {
        Payload::Value(value) => Ok(value.clone()),
        Payload::Packed(packed) => Json::parse(&packed.inflate()?),
    }
}

/// The payload's deflated bytes: as received, or deflated now if large. `None`: send it as JSON.
fn packed_bytes(payload: &Payload, compress: bool, json: Option<&str>) -> Option<Packed> {
    match (payload, json) {
        (Payload::Packed(packed), _) => Some(packed.clone()),
        (Payload::Value(_), Some(json)) if compress && utf16_len(json) > PACK_THRESHOLD => Some(Packed::deflate(json)),
        _ => None,
    }
}

/// The frame without its payload field, in order.
fn envelope(frame: &Frame) -> JsonMap {
    let fields = frame.fields().into_iter();
    fields.filter_map(|(name, field)| if let Field::Json(value) = field { Some((name, value)) } else { None }).collect()
}

/// For object transports (IPC, memory): a packed payload travels as `z: [field, base64]`.
pub fn pack_object(frame: &Frame, compress: bool) -> Json {
    if let Some((field, payload)) = frame.payload() {
        let json = match payload {
            Payload::Value(value) if compress => Some(value.to_string()),
            _ => None,
        };
        if let Some(bytes) = packed_bytes(payload, compress, json.as_deref()) {
            let mut map = envelope(frame);
            let encoded = Json::from(STANDARD.encode(bytes.bytes()));
            map.insert("z", Json::array(vec![Json::from(field.as_str()), encoded]));
            return Json::object(map);
        }
    }
    let fields = frame.fields().into_iter().map(|(name, field)| match field {
        Field::Json(value) => (name, value),
        Field::Payload(Payload::Value(value)) => (name, value.clone()),
        Field::Payload(Payload::Packed(packed)) => (name, Json::from(STANDARD.encode(packed.bytes()))),
    });
    Json::object(fields.collect())
}

/// The protocol error a transport closes the link with.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct BadMessage(pub &'static str);

/// Node's lenient base64: padding and unknown characters are ignored.
fn decode_base64(text: &str) -> Vec<u8> {
    let clean: String = text
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/')
        .collect();
    let usable = clean.len() - usize::from(clean.len() % 4 == 1);
    let config = GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireNone);
    GeneralPurpose::new(&alphabet::STANDARD, config).decode(&clean[..usable]).unwrap_or_default()
}

/// The received form of an object message: a frame, with a `z` payload restored as [`Packed`].
pub fn unpack_object(message: &Json) -> Result<Received, BadMessage> {
    let Some(map) = message.as_object() else { return check_frame(message, None) };
    let Some(z) = map.get("z") else { return check_frame(message, None) };
    let (field, text) = match z.as_array().map(|z| (z.first(), z.get(1))) {
        Some((Some(field), Some(Json::String(text)))) => (field.as_str().and_then(PayloadField::parse), text),
        _ => return Err(BadMessage("bad packed payload")),
    };
    let field = field.ok_or(BadMessage("bad packed payload"))?;
    let mut rest = map.clone();
    rest.remove("z");
    let bytes = Packed::new(decode_base64(text));
    received(&rest, Some((field, bytes)))
}

fn check_frame(message: &Json, packed: Option<(PayloadField, Packed)>) -> Result<Received, BadMessage> {
    match message.as_object() {
        Some(map) => received(map, packed),
        None => Err(BadMessage("not a frame")),
    }
}

fn received(map: &JsonMap, packed: Option<(PayloadField, Packed)>) -> Result<Received, BadMessage> {
    if !matches!(map.get("t"), Some(Json::String(_))) {
        return Err(BadMessage("not a frame"));
    }
    let raw = || {
        let mut raw = map.clone();
        if let Some((field, bytes)) = &packed {
            raw.insert(field.as_str(), Json::from(STANDARD.encode(bytes.bytes())));
        }
        Json::object(raw)
    };
    match Frame::parse(map, packed.clone()) {
        Some(frame) => Ok(Received::Frame(frame)),
        None => Ok(Received::Malformed(raw())),
    }
}

/// Parses a JSON text message; anything that is not an object with a string `t` is a protocol error.
pub fn parse_frame(data: &str) -> Result<Received, BadMessage> {
    let message = Json::parse(data).map_err(|_| BadMessage("not a frame"))?;
    unpack_object(&message)
}

/// A WebSocket message: text, or binary for a frame with a packed payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
}

impl Message {
    /// The size flow control counts: UTF-16 length of a text, bytes of a binary message.
    pub fn size(&self) -> usize {
        match self {
            Self::Text(text) => utf16_len(text),
            Self::Binary(bytes) => bytes.len(),
        }
    }
}

/// Splices `"field":json` in as the last field of an object's JSON text.
fn splice(head: &str, field: &str, json: &str) -> String {
    let mut text = String::with_capacity(head.len() + field.len() + json.len() + 4);
    text.push_str(&head[..head.len() - 1]);
    text.push_str(",\"");
    text.push_str(field);
    text.push_str("\":");
    text.push_str(json);
    text.push('}');
    text
}

/// For WebSockets: a frame with a packed payload is a binary message, `[envelope length: u32 BE][envelope JSON,
/// with z: field][deflated payload]`; any other frame is text, with the payload field last.
pub fn pack_message(frame: &Frame, compress: bool) -> Message {
    let Some((field, payload)) = frame.payload() else { return Message::Text(pack_object(frame, false).to_string()) };
    let json = match payload {
        Payload::Value(value) => Some(value.to_string()),
        Payload::Packed(_) => None,
    };
    let mut envelope = envelope(frame);
    match packed_bytes(payload, compress, json.as_deref()) {
        None => {
            let head = Json::object(envelope).to_string();
            Message::Text(splice(&head, field.as_str(), json.as_deref().unwrap_or("null")))
        }
        Some(bytes) => {
            envelope.insert("z", Json::from(field.as_str()));
            let head = Json::object(envelope).to_string();
            let mut message = Vec::with_capacity(4 + head.len() + bytes.bytes().len());
            message.extend_from_slice(&(head.len() as u32).to_be_bytes());
            message.extend_from_slice(head.as_bytes());
            message.extend_from_slice(bytes.bytes());
            Message::Binary(message)
        }
    }
}

/// The envelope of a binary message (without `z`), its payload field and the deflated bytes.
pub fn split_message(data: &[u8]) -> Result<(JsonMap, PayloadField, Packed), BadMessage> {
    let length: [u8; 4] = data.get(..4).and_then(|head| head.try_into().ok()).ok_or(BadMessage("bad envelope"))?;
    let end = (4 + u32::from_be_bytes(length) as usize).min(data.len());
    let text = String::from_utf8_lossy(&data[4..end]);
    let envelope = Json::parse(&text).map_err(|_| BadMessage("bad envelope"))?;
    let mut rest = envelope.as_object().ok_or(BadMessage("bad envelope"))?.clone();
    let field = rest.remove("z");
    let field = field.as_ref().and_then(Json::as_str).and_then(PayloadField::parse);
    let field = field.ok_or(BadMessage("bad packed payload"))?;
    Ok((rest, field, Packed::new(&data[end..])))
}

/// The received form of a binary message: the frame with its payload still packed.
pub fn unpack_message(data: &[u8]) -> Result<Received, BadMessage> {
    let (envelope, field, bytes) = split_message(data)?;
    received(&envelope, Some((field, bytes)))
}

/// The text frame of a binary (packed) message, inflated: for a transport that relays text frames only.
pub fn inflate_message(data: &[u8]) -> ChordResult<String> {
    let (envelope, field, bytes) = split_message(data).map_err(|error| ChordError::type_error(error.0))?;
    let head = Json::object(envelope).to_string();
    Ok(splice(&head, field.as_str(), &bytes.inflate()?))
}
