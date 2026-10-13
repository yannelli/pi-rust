//! Protocol version 8 envelopes and their strict JSON shapes.

use pi_chord::{Json, JsonMap};

/// The only protocol version this implementation speaks.
pub const PROTOCOL_VERSION: u32 = 8;

/// A canonical lowercase UUIDv4 naming one logical server.
pub type ServerId = String;
/// A non-empty opaque error code.
pub type ProtocolErrorCode = String;

/// Whether `value` is a canonical lowercase UUIDv4.
pub fn is_server_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, &byte)| match index {
        8 | 13 | 18 | 23 => byte == b'-',
        14 => byte == b'4',
        19 => matches!(byte, b'8' | b'9' | b'a' | b'b'),
        _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
    })
}

/// A bounded error carried by responses and handshake failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
}

impl ProtocolError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
}

/// Must be the first frame a client sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientHello {
    pub version: u64,
}

impl Default for ClientHello {
    fn default() -> Self {
        Self { version: u64::from(PROTOCOL_VERSION) }
    }
}

/// A Session call route, fenced to one server, durable Session, and live attachment.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionTarget {
    pub server_id: ServerId,
    pub session_id: String,
    pub attachment_id: String,
}

/// A server-wide or Session route.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RpcTarget {
    Server { server_id: ServerId },
    Session(SessionTarget),
}

impl RpcTarget {
    pub fn server(server_id: impl Into<String>) -> Self {
        Self::Server { server_id: server_id.into() }
    }

    pub fn server_id(&self) -> &str {
        match self {
            Self::Server { server_id } | Self::Session(SessionTarget { server_id, .. }) => server_id,
        }
    }

    pub fn session(&self) -> Option<&SessionTarget> {
        match self {
            Self::Server { .. } => None,
            Self::Session(target) => Some(target),
        }
    }
}

impl From<SessionTarget> for RpcTarget {
    fn from(target: SessionTarget) -> Self {
        Self::Session(target)
    }
}

/// One correlated call with an opaque strict-JSON payload.
#[derive(Clone, Debug, PartialEq)]
pub struct RequestEnvelope {
    pub id: String,
    pub target: RpcTarget,
    pub call: Json,
}

/// Cancels one active request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelEnvelope {
    pub id: String,
    pub target: RpcTarget,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClientMessage {
    Hello(ClientHello),
    Request(RequestEnvelope),
    Cancel(CancelEnvelope),
}

/// The server's successful handshake reply; its version is always [`PROTOCOL_VERSION`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerHello {
    pub server_id: ServerId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerHelloError {
    pub error: ProtocolError,
}

/// A response: `Ok` with an optional result, or a bounded error.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponseEnvelope {
    pub id: String,
    pub result: Result<Option<Json>, ProtocolError>,
}

/// An opaque update for one service subscription.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceEventEnvelope {
    pub subscription_id: String,
    pub update: Json,
}

/// Out-of-band update to this presentation's selected Session route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentEnvelope {
    pub attachment: Option<SessionTarget>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ServerMessage {
    Hello(ServerHello),
    HelloError(ServerHelloError),
    Response(ResponseEnvelope),
    ServiceUpdate(ServiceEventEnvelope),
    Attachment(AttachmentEnvelope),
}

fn object(entries: Vec<(&str, Json)>) -> Json {
    let mut map = JsonMap::new();
    for (key, value) in entries {
        map.insert(key, value);
    }
    Json::object(map)
}

impl ProtocolError {
    pub fn to_json(&self) -> Json {
        object(vec![("code", self.code.as_str().into()), ("message", self.message.as_str().into())])
    }
}

impl SessionTarget {
    pub fn to_json(&self) -> Json {
        object(vec![
            ("serverId", self.server_id.as_str().into()),
            ("sessionId", self.session_id.as_str().into()),
            ("attachmentId", self.attachment_id.as_str().into()),
        ])
    }
}

impl RpcTarget {
    pub fn to_json(&self) -> Json {
        match self {
            Self::Server { server_id } => object(vec![("serverId", server_id.as_str().into())]),
            Self::Session(target) => target.to_json(),
        }
    }
}

impl ClientMessage {
    /// The upstream object shape, with keys in schema order.
    pub fn to_json(&self) -> Json {
        match self {
            Self::Hello(hello) => object(vec![("type", "hello".into()), ("version", hello.version.into())]),
            Self::Request(request) => object(vec![
                ("type", "request".into()),
                ("id", request.id.as_str().into()),
                ("target", request.target.to_json()),
                ("call", request.call.clone()),
            ]),
            Self::Cancel(cancel) => object(vec![
                ("type", "cancel".into()),
                ("id", cancel.id.as_str().into()),
                ("target", cancel.target.to_json()),
            ]),
        }
    }
}

impl ServerMessage {
    /// The upstream object shape, with keys in schema order.
    pub fn to_json(&self) -> Json {
        match self {
            Self::Hello(hello) => object(vec![
                ("type", "hello".into()),
                ("version", PROTOCOL_VERSION.into()),
                ("serverId", hello.server_id.as_str().into()),
            ]),
            Self::HelloError(error) => object(vec![("type", "hello_error".into()), ("error", error.error.to_json())]),
            Self::Response(response) => {
                let mut entries = vec![("type", "response".into()), ("id", response.id.as_str().into())];
                match &response.result {
                    Ok(result) => {
                        entries.push(("ok", true.into()));
                        if let Some(result) = result {
                            entries.push(("result", result.clone()));
                        }
                    }
                    Err(error) => {
                        entries.push(("ok", false.into()));
                        entries.push(("error", error.to_json()));
                    }
                }
                object(entries)
            }
            Self::ServiceUpdate(event) => object(vec![
                ("type", "service_update".into()),
                ("subscriptionId", event.subscription_id.as_str().into()),
                ("update", event.update.clone()),
            ]),
            Self::Attachment(envelope) => object(vec![
                ("type", "attachment".into()),
                ("attachment", envelope.attachment.as_ref().map_or(Json::Null, SessionTarget::to_json)),
            ]),
        }
    }
}

/// Strict-schema parsing. Every object rejects unknown properties.
pub(crate) mod parse {
    use super::*;

    fn keys(map: &JsonMap, required: &[&str], optional: &[&str]) -> Option<()> {
        let complete = required.iter().all(|key| map.contains_key(key));
        let known = map.keys().all(|key| required.contains(&&**key) || optional.contains(&&**key));
        (complete && known).then_some(())
    }

    fn id(value: Option<&Json>) -> Option<String> {
        value?.as_str().filter(|text| !text.is_empty()).map(str::to_string)
    }

    fn server_id(value: Option<&Json>) -> Option<String> {
        value?.as_str().filter(|text| is_server_id(text)).map(str::to_string)
    }

    fn error(value: &Json) -> Option<ProtocolError> {
        let map = value.as_object()?;
        keys(map, &["code", "message"], &[])?;
        Some(ProtocolError { code: id(map.get("code"))?, message: map.get("message")?.as_str()?.to_string() })
    }

    fn session_target(value: &Json) -> Option<SessionTarget> {
        let map = value.as_object()?;
        keys(map, &["serverId", "sessionId", "attachmentId"], &[])?;
        Some(SessionTarget {
            server_id: server_id(map.get("serverId"))?,
            session_id: id(map.get("sessionId"))?,
            attachment_id: id(map.get("attachmentId"))?,
        })
    }

    fn target(value: &Json) -> Option<RpcTarget> {
        let map = value.as_object()?;
        if map.len() == 1 {
            keys(map, &["serverId"], &[])?;
            return Some(RpcTarget::Server { server_id: server_id(map.get("serverId"))? });
        }
        session_target(value).map(RpcTarget::Session)
    }

    pub(crate) fn client_message(value: &Json) -> Option<ClientMessage> {
        let map = value.as_object()?;
        match map.get("type")?.as_str()? {
            "hello" => {
                keys(map, &["type", "version"], &[])?;
                Some(ClientMessage::Hello(ClientHello { version: map.get("version")?.as_u64()? }))
            }
            "request" => {
                keys(map, &["type", "id", "target", "call"], &[])?;
                Some(ClientMessage::Request(RequestEnvelope {
                    id: id(map.get("id"))?,
                    target: target(map.get("target")?)?,
                    call: map.get("call")?.clone(),
                }))
            }
            "cancel" => {
                keys(map, &["type", "id", "target"], &[])?;
                Some(ClientMessage::Cancel(CancelEnvelope {
                    id: id(map.get("id"))?,
                    target: target(map.get("target")?)?,
                }))
            }
            _ => None,
        }
    }

    pub(crate) fn server_message(value: &Json) -> Option<ServerMessage> {
        let map = value.as_object()?;
        match map.get("type")?.as_str()? {
            "hello" => {
                keys(map, &["type", "version", "serverId"], &[])?;
                (map.get("version")?.as_f64()? == f64::from(PROTOCOL_VERSION)).then_some(())?;
                Some(ServerMessage::Hello(ServerHello { server_id: server_id(map.get("serverId"))? }))
            }
            "hello_error" => {
                keys(map, &["type", "error"], &[])?;
                Some(ServerMessage::HelloError(ServerHelloError { error: error(map.get("error")?)? }))
            }
            "response" => {
                let response_id = id(map.get("id"))?;
                let result = match map.get("ok")?.as_bool()? {
                    true => {
                        keys(map, &["type", "id", "ok"], &["result"])?;
                        Ok(map.get("result").cloned())
                    }
                    false => {
                        keys(map, &["type", "id", "ok", "error"], &[])?;
                        Err(error(map.get("error")?)?)
                    }
                };
                Some(ServerMessage::Response(ResponseEnvelope { id: response_id, result }))
            }
            "service_update" => {
                keys(map, &["type", "subscriptionId", "update"], &[])?;
                Some(ServerMessage::ServiceUpdate(ServiceEventEnvelope {
                    subscription_id: id(map.get("subscriptionId"))?,
                    update: map.get("update")?.clone(),
                }))
            }
            "attachment" => {
                keys(map, &["type", "attachment"], &[])?;
                let attachment = match map.get("attachment")? {
                    Json::Null => None,
                    value => Some(session_target(value)?),
                };
                Some(ServerMessage::Attachment(AttachmentEnvelope { attachment }))
            }
            _ => None,
        }
    }
}
