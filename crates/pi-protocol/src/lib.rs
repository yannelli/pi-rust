//! Runtime-neutral routed envelopes, CBOR encoding, and byte-stream framing for the experimental
//! pi protocol. A Rust port of upstream `@earendil-works/pi-protocol`; the wire bytes match it.

pub mod cbor;
mod codec;
mod framing;
mod protocol;

pub use cbor::{
    CborError, CborOptions, CborValue, DEFAULT_MAX_CBOR_BYTE_LENGTH, DEFAULT_MAX_CBOR_CONTAINER_LENGTH,
    DEFAULT_MAX_CBOR_DEPTH, decode_cbor, decode_json_cbor, encode_cbor, encode_json_cbor,
};
pub use codec::{
    ClientMessageDecoder, ProtocolValidationError, ServerMessageDecoder, encode_client_message, encode_server_message,
    is_supported_protocol_version, parse_client_message, parse_server_message,
};
pub use framing::{DEFAULT_MAX_FRAME_LENGTH, FrameDecoder, FrameDecoderOptions, FrameError, encode_frame};
pub use protocol::{
    AttachmentEnvelope, CancelEnvelope, ClientHello, ClientMessage, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
    RequestEnvelope, ResponseEnvelope, RpcTarget, ServerHello, ServerHelloError, ServerId, ServerMessage,
    ServiceEventEnvelope, SessionTarget, is_server_id,
};
