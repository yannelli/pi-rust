//! Validated message codecs over CBOR and framing.

use pi_chord::Json;

use crate::cbor::{CborOptions, decode_cbor, encode_json_cbor};
use crate::framing::{FrameDecoder, FrameDecoderOptions, encode_frame};
use crate::protocol::{ClientMessage, PROTOCOL_VERSION, ServerMessage, parse};

/// An envelope violation, malformed CBOR, or invalid framing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ProtocolValidationError(pub String);

impl ProtocolValidationError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Validate a strict JSON value as a client message.
pub fn parse_client_message(value: &Json) -> Result<ClientMessage, ProtocolValidationError> {
    parse::client_message(value).ok_or_else(|| ProtocolValidationError::new("Invalid client protocol message"))
}

/// Validate a strict JSON value as a server message.
pub fn parse_server_message(value: &Json) -> Result<ServerMessage, ProtocolValidationError> {
    parse::server_message(value).ok_or_else(|| ProtocolValidationError::new("Invalid server protocol message"))
}

fn bounded(message: &str) -> String {
    if message.chars().count() <= 500 {
        return message.to_string();
    }
    let prefix: String = message.chars().take(497).collect();
    format!("{prefix}...")
}

fn encode_message(value: Json, kind: &str, options: &FrameDecoderOptions) -> Result<Vec<u8>, ProtocolValidationError> {
    let cbor_options = CborOptions { max_byte_length: Some(options.max_frame_length()), ..CborOptions::default() };
    let failure = |message: String| {
        ProtocolValidationError(format!("Unable to encode {kind} protocol message: {}", bounded(&message)))
    };
    let payload = encode_json_cbor(&value, &cbor_options).map_err(|error| failure(error.0))?;
    encode_frame(&payload).map_err(|error| failure(error.0))
}

/// Validate and encode one complete length-prefixed client message.
pub fn encode_client_message(
    message: &ClientMessage,
    options: &FrameDecoderOptions,
) -> Result<Vec<u8>, ProtocolValidationError> {
    let value = message.to_json();
    parse_client_message(&value)?;
    encode_message(value, "client", options)
}

/// Validate and encode one complete length-prefixed server message.
pub fn encode_server_message(
    message: &ServerMessage,
    options: &FrameDecoderOptions,
) -> Result<Vec<u8>, ProtocolValidationError> {
    let value = message.to_json();
    parse_server_message(&value)?;
    encode_message(value, "server", options)
}

struct ValidatedDecoder<T> {
    failed: bool,
    frames: FrameDecoder,
    kind: &'static str,
    max_frame_length: u32,
    parse: fn(&Json) -> Result<T, ProtocolValidationError>,
}

impl<T> ValidatedDecoder<T> {
    fn new(
        kind: &'static str,
        parse: fn(&Json) -> Result<T, ProtocolValidationError>,
        options: FrameDecoderOptions,
    ) -> Self {
        Self {
            failed: false,
            frames: FrameDecoder::new(options),
            kind,
            max_frame_length: options.max_frame_length(),
            parse,
        }
    }

    fn failed_error(&self) -> ProtocolValidationError {
        ProtocolValidationError(format!("{} message decoder has failed", self.kind))
    }

    fn decode_frame(&self, frame: &[u8]) -> Result<T, ProtocolValidationError> {
        let options = CborOptions { max_byte_length: Some(self.max_frame_length), ..CborOptions::default() };
        let invalid = |message: &str| {
            ProtocolValidationError(format!("Invalid {} protocol frame: {}", self.kind, bounded(message)))
        };
        let value = decode_cbor(frame, &options).map_err(|error| invalid(&error.0))?;
        let json = match value.to_json() {
            Some(json) => json,
            None => return Err(ProtocolValidationError(format!("Invalid {} protocol message", self.kind))),
        };
        (self.parse)(&json)
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Vec<T>, ProtocolValidationError> {
        if self.failed {
            return Err(self.failed_error());
        }
        let result = match self.frames.push(chunk) {
            Ok(frames) => frames.iter().map(|frame| self.decode_frame(frame)).collect(),
            Err(error) => {
                Err(ProtocolValidationError(format!("Invalid {} protocol frame: {}", self.kind, bounded(&error.0))))
            }
        };
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn end(&mut self) -> Result<(), ProtocolValidationError> {
        if self.failed {
            return Err(self.failed_error());
        }
        self.frames.end().map_err(|error| {
            self.failed = true;
            ProtocolValidationError(format!("Invalid {} protocol framing: {}", self.kind, bounded(&error.0)))
        })
    }
}

/// Incrementally decodes and validates framed client messages.
pub struct ClientMessageDecoder(ValidatedDecoder<ClientMessage>);

impl Default for ClientMessageDecoder {
    fn default() -> Self {
        Self::new(FrameDecoderOptions::default())
    }
}

impl ClientMessageDecoder {
    pub fn new(options: FrameDecoderOptions) -> Self {
        Self(ValidatedDecoder::new("client", parse_client_message, options))
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ClientMessage>, ProtocolValidationError> {
        self.0.push(chunk)
    }

    pub fn end(&mut self) -> Result<(), ProtocolValidationError> {
        self.0.end()
    }
}

/// Incrementally decodes and validates framed server messages.
pub struct ServerMessageDecoder(ValidatedDecoder<ServerMessage>);

impl Default for ServerMessageDecoder {
    fn default() -> Self {
        Self::new(FrameDecoderOptions::default())
    }
}

impl ServerMessageDecoder {
    pub fn new(options: FrameDecoderOptions) -> Self {
        Self(ValidatedDecoder::new("server", parse_server_message, options))
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ServerMessage>, ProtocolValidationError> {
        self.0.push(chunk)
    }

    pub fn end(&mut self) -> Result<(), ProtocolValidationError> {
        self.0.end()
    }
}

/// Whether a client hello version is the one this implementation speaks.
pub fn is_supported_protocol_version(version: u64) -> bool {
    version == u64::from(PROTOCOL_VERSION)
}
