//! Frames as JSON fields in upstream's object-literal order. `undefined` fields are left out, as `JSON.stringify`
//! leaves them out.

use super::{Frame, Payload, PeerKind, RootRef, WireError, WireReloadOptions, WireReloadReport};
use crate::json::{Json, JsonMap};

/// The field that holds a frame's payload: `args` of a call, `value` of a result or state frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadField {
    Args,
    Value,
}

impl PayloadField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Args => "args",
            Self::Value => "value",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "args" => Some(Self::Args),
            "value" => Some(Self::Value),
            _ => None,
        }
    }
}

/// One field of a frame: plain JSON, or the payload.
pub(crate) enum Field<'a> {
    Json(Json),
    Payload(&'a Payload),
}

fn number(value: impl Into<f64>) -> Json {
    Json::from(value.into())
}

fn integer(value: u64) -> Json {
    Json::from(value as f64)
}

fn strings(items: &[String]) -> Json {
    Json::array(items.iter().map(|item| Json::from(item.as_str())).collect())
}

fn object(entries: impl IntoIterator<Item = (&'static str, Json)>) -> Json {
    Json::object(entries.into_iter().collect())
}

impl WireError {
    pub fn to_json(&self) -> Json {
        let mut map = JsonMap::new();
        map.insert("code", Json::from(self.code.as_str()));
        map.insert("message", Json::from(self.message.as_str()));
        if let Some(reason) = &self.reason {
            map.insert("reason", Json::from(reason.as_str()));
        }
        Json::object(map)
    }
}

impl RootRef {
    pub fn to_json(&self) -> Json {
        object([("rootId", Json::from(self.root_id.as_str())), ("codeVersion", integer(self.code_version))])
    }
}

impl WireReloadOptions {
    pub fn to_json(&self) -> Json {
        let mut map = JsonMap::new();
        if let Some(remove) = &self.remove {
            map.insert("remove", strings(remove));
        }
        if let Some(deadline) = self.gate_deadline {
            map.insert("gateDeadline", number(deadline));
        }
        if let Some(deadline) = self.retire_deadline {
            map.insert("retireDeadline", number(deadline));
        }
        Json::object(map)
    }
}

impl WireReloadReport {
    pub fn to_json(&self) -> Json {
        object([
            ("codeVersion", number(self.code_version as f64)),
            ("previous", number(self.previous as f64)),
            ("changed", strings(&self.changed)),
            ("removed", strings(&self.removed)),
            ("prepared", strings(&self.prepared)),
            ("gated", strings(&self.gated)),
        ])
    }
}

/// Collects fields in order, skipping absent ones.
struct Fields<'a>(Vec<(&'static str, Field<'a>)>);

impl<'a> Fields<'a> {
    fn json(&mut self, name: &'static str, value: Json) -> &mut Self {
        self.0.push((name, Field::Json(value)));
        self
    }

    fn some(&mut self, name: &'static str, value: Option<Json>) -> &mut Self {
        if let Some(value) = value {
            self.json(name, value);
        }
        self
    }

    fn id(&mut self, name: &'static str, value: u64) -> &mut Self {
        self.json(name, integer(value))
    }

    fn text(&mut self, name: &'static str, value: &str) -> &mut Self {
        self.json(name, Json::from(value))
    }

    fn payload(&mut self, name: &'static str, value: Option<&'a Payload>) -> &mut Self {
        if let Some(value) = value {
            self.0.push((name, Field::Payload(value)));
        }
        self
    }
}

impl Frame {
    /// The payload field and its value, when the frame carries one.
    pub(crate) fn payload(&self) -> Option<(PayloadField, &Payload)> {
        match self {
            Self::Call(call) => call.args.as_ref().map(|args| (PayloadField::Args, args)),
            Self::Result(result) => result.value.as_ref().map(|value| (PayloadField::Value, value)),
            Self::State(state) => state.value.as_ref().map(|value| (PayloadField::Value, value)),
            _ => None,
        }
    }

    /// The fields in upstream's object-literal order, `t` first.
    pub(crate) fn fields(&self) -> Vec<(&'static str, Field<'_>)> {
        let mut out = Fields(Vec::with_capacity(8));
        out.text("t", self.tag());
        match self {
            Self::Hello(hello) => {
                let kind = match hello.kind {
                    PeerKind::Host => "host",
                    PeerKind::Client => "client",
                };
                out.some("protocol", hello.protocol.map(integer)).text("name", &hello.name).text("kind", kind);
                out.id("codeVersion", hello.code_version).some("acks", hello.acks.map(Json::from));
            }
            Self::Ack { n } => {
                out.id("n", *n);
            }
            Self::Hold(hold) => {
                out.id("id", hold.id).text("at", &hold.at).json("token", hold.token.to_json().clone());
                out.json("input", hold.input.clone()).some("principal", hold.principal.clone());
                out.some("origin", hold.origin.as_deref().map(Json::from));
                out.some("state", (!hold.state).then_some(Json::Bool(false)));
                out.some("again", hold.again.then_some(Json::Bool(true)));
            }
            Self::Release { id } | Self::Cancel { id } => {
                out.id("id", *id);
            }
            Self::Shape(shape) => {
                out.id("id", shape.id).some("variant", shape.variant.as_deref().map(Json::from));
                out.json("members", strings(&shape.members)).text("contract", &shape.contract);
                out.json("state", strings(&shape.state));
            }
            Self::Gone { id, error } => {
                out.id("id", *id).json("error", error.to_json());
            }
            Self::Call(call) => {
                out.id("id", call.id).id("hold", call.hold).text("member", &call.member);
                out.payload("args", call.args.as_ref()).some("root", call.root.as_ref().map(RootRef::to_json));
            }
            Self::Result(result) => {
                out.id("id", result.id).payload("value", result.value.as_ref());
                out.some("error", result.error.as_ref().map(WireError::to_json));
            }
            Self::Sub { hold } | Self::Unsub { hold } => {
                out.id("hold", *hold);
            }
            Self::Resync { hold, m } => {
                out.id("hold", *hold).text("m", m);
            }
            Self::State(state) => {
                out.id("hold", state.hold).text("m", &state.m).text("as", state.kind.as_str());
                out.payload("value", state.value.as_ref());
                let ops =
                    state.ops.as_ref().map(|ops| Json::array(ops.iter().map(|op| Json::array(op.clone())).collect()));
                out.some("ops", ops).some("error", state.error.as_ref().map(WireError::to_json));
            }
            Self::Ping { n } | Self::Pong { n } => {
                out.json("n", number(*n as f64));
            }
            Self::Bye { reason } => {
                out.some("reason", reason.as_deref().map(Json::from));
            }
            Self::RootEnd { root_id } => {
                out.text("rootId", root_id);
            }
            Self::Invalidate { pattern } => {
                out.json("pattern", Json::object(pattern.clone()));
            }
            Self::Prepare(prepare) => {
                out.id("reload", prepare.reload).id("codeVersion", prepare.code_version);
                out.json("plugins", strings(&prepare.plugins)).json("options", prepare.options.to_json());
            }
            Self::Vote(vote) => {
                out.id("reload", vote.reload).some("error", vote.error.as_ref().map(WireError::to_json));
                out.some("report", vote.report.as_ref().map(WireReloadReport::to_json));
            }
            Self::Commit { reload } | Self::Abort { reload } => {
                out.id("reload", *reload);
            }
            Self::Retire { code_version } => {
                out.id("codeVersion", *code_version);
            }
        }
        out.0
    }
}
