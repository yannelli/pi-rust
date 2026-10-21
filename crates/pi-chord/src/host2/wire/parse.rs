//! Frame validation: the verdicts of upstream's `isFrame` (TypeBox `Value.Check` of the frame schema).

use super::write::PayloadField;
use super::{
    Call, CallResult, Frame, Hello, Hold, Payload, PeerKind, Prepare, RootRef, Shape, StateFrame, StateKind, Vote,
    WireError, WireReloadOptions, WireReloadReport,
};
use crate::host2::contract::{Description, safe_integer};
use crate::host2::errors::ChordErrorCode;
use crate::host2::link::Packed;
use crate::json::{Json, JsonMap};

/// A frame that passes validation; anything else is a protocol violation.
pub fn is_frame(value: &Json) -> bool {
    Frame::from_json(value).is_some()
}

fn integer(value: &Json) -> Option<i64> {
    safe_integer(value)
}

fn at_least(value: &Json, minimum: i64) -> Option<u64> {
    integer(value).filter(|value| *value >= minimum).map(|value| value as u64)
}

fn id(value: &Json) -> Option<u64> {
    at_least(value, 1)
}

fn string(value: &Json) -> Option<String> {
    value.as_str().map(String::from)
}

fn name(value: &Json) -> Option<String> {
    value.as_str().filter(|text| !text.is_empty()).map(String::from)
}

fn list<T>(value: &Json, item: impl Fn(&Json) -> Option<T>) -> Option<Vec<T>> {
    value.as_array()?.iter().map(item).collect()
}

fn wire_error(value: &Json) -> Option<WireError> {
    let map = value.as_object()?;
    Some(WireError {
        code: map.get("code").and_then(Json::as_str).and_then(ChordErrorCode::parse)?,
        message: string(map.get("message")?)?,
        reason: optional(map, "reason", string)?,
    })
}

fn root_ref(value: &Json) -> Option<RootRef> {
    let map = value.as_object()?;
    Some(RootRef { root_id: name(map.get("rootId")?)?, code_version: at_least(map.get("codeVersion")?, 1)? })
}

fn reload_options(value: &Json) -> Option<WireReloadOptions> {
    let map = value.as_object()?;
    Some(WireReloadOptions {
        remove: optional(map, "remove", |value| list(value, string))?,
        gate_deadline: optional(map, "gateDeadline", Json::as_f64)?,
        retire_deadline: optional(map, "retireDeadline", Json::as_f64)?,
    })
}

fn reload_report(value: &Json) -> Option<WireReloadReport> {
    let map = value.as_object()?;
    let strings = |key: &str| list(map.get(key)?, string);
    Some(WireReloadReport {
        code_version: integer(map.get("codeVersion")?)?,
        previous: integer(map.get("previous")?)?,
        changed: strings("changed")?,
        removed: strings("removed")?,
        prepared: strings("prepared")?,
        gated: strings("gated")?,
    })
}

/// An optional field: `Some(None)` when absent, `None` when present but invalid.
fn optional<T>(map: &JsonMap, key: &str, parse: impl Fn(&Json) -> Option<T>) -> Option<Option<T>> {
    match map.get(key) {
        None => Some(None),
        Some(value) => parse(value).map(Some),
    }
}

/// A literal-valued optional flag (`state: false`, `again: true`): absent is the default.
fn flag(map: &JsonMap, key: &str, literal: bool) -> Option<bool> {
    match map.get(key) {
        None => Some(false),
        Some(Json::Bool(value)) if *value == literal => Some(true),
        Some(_) => None,
    }
}

/// The payload field: packed bytes when the message carried them for this field, otherwise the JSON value.
fn payload(map: &JsonMap, field: PayloadField, packed: &Option<(PayloadField, Packed)>) -> Option<Payload> {
    match packed {
        Some((packed_field, bytes)) if *packed_field == field => Some(Payload::Packed(bytes.clone())),
        _ => map.get(field.as_str()).cloned().map(Payload::Value),
    }
}

impl Frame {
    /// Validates a frame received as JSON.
    pub fn from_json(value: &Json) -> Option<Self> {
        Self::parse(value.as_object()?, None)
    }

    /// Validates a frame's fields; `packed` is a payload that arrived deflated (it replaces that field).
    pub(crate) fn parse(map: &JsonMap, packed: Option<(PayloadField, Packed)>) -> Option<Self> {
        let get = |key: &str| map.get(key);
        let frame = match get("t")?.as_str()? {
            "hello" => Self::Hello(Hello {
                protocol: optional(map, "protocol", |value| at_least(value, 1))?,
                name: name(get("name")?)?,
                kind: match get("kind")?.as_str()? {
                    "host" => PeerKind::Host,
                    "client" => PeerKind::Client,
                    _ => return None,
                },
                code_version: at_least(get("codeVersion")?, 0)?,
                acks: optional(map, "acks", Json::as_bool)?,
            }),
            "ack" => Self::Ack { n: id(get("n")?)? },
            "hold" => Self::Hold(Box::new(Hold {
                id: id(get("id")?)?,
                at: name(get("at")?)?,
                token: Description::from_json(get("token")?.clone()).ok()?,
                input: get("input")?.clone(),
                principal: get("principal").cloned(),
                origin: optional(map, "origin", name)?,
                state: !flag(map, "state", false)?,
                again: flag(map, "again", true)?,
            })),
            "release" => Self::Release { id: id(get("id")?)? },
            "shape" => Self::Shape(Shape {
                id: id(get("id")?)?,
                variant: optional(map, "variant", string)?,
                members: list(get("members")?, string)?,
                contract: string(get("contract")?)?,
                state: list(get("state")?, name)?,
            }),
            "gone" => Self::Gone { id: id(get("id")?)?, error: wire_error(get("error")?)? },
            "call" => Self::Call(Call {
                id: id(get("id")?)?,
                hold: id(get("hold")?)?,
                member: name(get("member")?)?,
                args: payload(map, PayloadField::Args, &packed),
                root: optional(map, "root", root_ref)?,
            }),
            "result" => Self::Result(CallResult {
                id: id(get("id")?)?,
                value: payload(map, PayloadField::Value, &packed),
                error: optional(map, "error", wire_error)?,
            }),
            "cancel" => Self::Cancel { id: id(get("id")?)? },
            "sub" => Self::Sub { hold: id(get("hold")?)? },
            "unsub" => Self::Unsub { hold: id(get("hold")?)? },
            "resync" => Self::Resync { hold: id(get("hold")?)?, m: name(get("m")?)? },
            "state" => Self::State(StateFrame {
                hold: id(get("hold")?)?,
                m: name(get("m")?)?,
                kind: match get("as")?.as_str()? {
                    "snap" => StateKind::Snap,
                    "reset" => StateKind::Reset,
                    "ops" => StateKind::Ops,
                    "error" => StateKind::Error,
                    _ => return None,
                },
                value: payload(map, PayloadField::Value, &packed),
                ops: optional(map, "ops", |ops| list(ops, |op| op.as_array().cloned()))?,
                error: optional(map, "error", wire_error)?,
            }),
            "ping" => Self::Ping { n: integer(get("n")?)? },
            "pong" => Self::Pong { n: integer(get("n")?)? },
            "bye" => Self::Bye { reason: optional(map, "reason", string)? },
            "root_end" => Self::RootEnd { root_id: name(get("rootId")?)? },
            "invalidate" => Self::Invalidate { pattern: get("pattern")?.as_object()?.clone() },
            "prepare" => Self::Prepare(Prepare {
                reload: id(get("reload")?)?,
                code_version: at_least(get("codeVersion")?, 1)?,
                plugins: list(get("plugins")?, string)?,
                options: reload_options(get("options")?)?,
            }),
            "vote" => Self::Vote(Vote {
                reload: id(get("reload")?)?,
                error: optional(map, "error", wire_error)?,
                report: optional(map, "report", reload_report)?,
            }),
            "commit" => Self::Commit { reload: id(get("reload")?)? },
            "retire" => Self::Retire { code_version: at_least(get("codeVersion")?, 1)? },
            "abort" => Self::Abort { reload: id(get("reload")?)? },
            _ => return None,
        };
        Some(frame)
    }
}
