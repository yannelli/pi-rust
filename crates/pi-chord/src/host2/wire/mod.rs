//! Port of upstream `wire.ts`: the JSON frames between two hosts (or a host and a client). [`Frame::parse`] accepts
//! what upstream's `isFrame` accepts (unknown fields too, as TypeBox does); the writers emit fields in upstream's
//! object-literal order, so Rust and TypeScript hosts exchange identical text.

mod parse;
mod write;

pub use parse::is_frame;
pub(crate) use write::Field;
pub use write::PayloadField;

use super::contract::Description;
use super::errors::{ChordErrorCode, ErrorOptions};
use super::link::Packed;
use crate::error::ChordError;
use crate::json::{Json, JsonMap};

/// The link protocol version, announced in `hello`. Peers of different versions do not talk: the side that notices
/// closes the link with reason `protocol-version <its version>`, so the other side learns it too.
pub const PROTOCOL_VERSION: u64 = 4;

/// The root a call belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootRef {
    pub root_id: String,
    pub code_version: u64,
}

/// An error as it crosses a link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireError {
    pub code: ChordErrorCode,
    pub message: String,
    pub reason: Option<String>,
}

/// Reload options a parent sends with `prepare`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WireReloadOptions {
    pub remove: Option<Vec<String>>,
    pub gate_deadline: Option<f64>,
    pub retire_deadline: Option<f64>,
}

/// A child's prepared reload, sent with its vote (same fields as `ReloadReport`, without children).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WireReloadReport {
    pub code_version: i64,
    pub previous: i64,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    pub prepared: Vec<String>,
    pub gated: Vec<String>,
}

/// A frame payload (call args, a result value, a state value): JSON, or deflated bytes a relay forwards unread.
#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    Value(Json),
    Packed(Packed),
}

impl From<Json> for Payload {
    fn from(value: Json) -> Self {
        Self::Value(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerKind {
    Host,
    Client,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hello {
    /// The link protocol version ([`PROTOCOL_VERSION`]; absent: 1).
    pub protocol: Option<u64>,
    pub name: String,
    pub kind: PeerKind,
    pub code_version: u64,
    /// The sender acknowledges what it receives (`ack`). Flow control applies only when both hellos say so.
    pub acks: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hold {
    pub id: u64,
    /// Target host name, resolved by the consumer from the token's `host` and `input`.
    pub at: String,
    /// The consumer's contract token; the provider checks that it covers it.
    pub token: Description,
    /// Normalized with the consumer's contract.
    pub input: Json,
    /// The caller identity a relay forwards. Honored only on frames from the parent link.
    pub principal: Option<Json>,
    /// The client connection the hold comes from, forwarded by relays.
    pub origin: Option<String>,
    /// False: no state for this hold until `sub` (sent as `state: false`).
    pub state: bool,
    /// The consumer already has values (a re-hold after a link loss; sent as `again: true`).
    pub again: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    pub id: u64,
    pub variant: Option<String>,
    pub members: Vec<String>,
    pub contract: String,
    /// The state members whose snapshots follow this frame.
    pub state: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub id: u64,
    pub hold: u64,
    pub member: String,
    pub args: Option<Payload>,
    /// Absent: the receiving host starts a root (browsers never carry roots).
    pub root: Option<RootRef>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CallResult {
    pub id: u64,
    pub value: Option<Payload>,
    pub error: Option<WireError>,
}

/// What a `state` frame carries (`as` on the wire).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateKind {
    Snap,
    Reset,
    Ops,
    Error,
}

impl StateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Snap => "snap",
            Self::Reset => "reset",
            Self::Ops => "ops",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StateFrame {
    pub hold: u64,
    pub m: String,
    pub kind: StateKind,
    /// `snap`/`reset` without a value: the value is `undefined`.
    pub value: Option<Payload>,
    /// Encoded with the stream's delta encoder; the decoder validates each op.
    pub ops: Option<Vec<Vec<Json>>>,
    pub error: Option<WireError>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Prepare {
    pub reload: u64,
    pub code_version: u64,
    pub plugins: Vec<String>,
    pub options: WireReloadOptions,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Vote {
    pub reload: u64,
    pub error: Option<WireError>,
    pub report: Option<WireReloadReport>,
}

/// Every frame type, by direction. Consumer to provider: hold, release, call, cancel, sub, unsub, resync. Provider
/// to consumer: shape, gone, result, state. Parent to child: invalidate, prepare, commit, abort, retire; child to
/// parent: vote. Both: hello, ack, ping, pong, bye, root_end. Integers stay within JavaScript's safe range.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Hello(Hello),
    /// Every frame received so far except `ack` frames, counted from the link's start, once processed.
    Ack {
        n: u64,
    },
    Hold(Box<Hold>),
    Release {
        id: u64,
    },
    Shape(Shape),
    Gone {
        id: u64,
        error: WireError,
    },
    Call(Call),
    Result(CallResult),
    Cancel {
        id: u64,
    },
    /// Starts the state of a hold opened with `state: false`.
    Sub {
        hold: u64,
    },
    Unsub {
        hold: u64,
    },
    /// Sends member `m` again as a reset (a relay that had to drop what waited for it).
    Resync {
        hold: u64,
        m: String,
    },
    State(StateFrame),
    Ping {
        n: i64,
    },
    Pong {
        n: i64,
    },
    /// Ends the link (consumed by the link itself). `protocol-version <n>`: the sender speaks version n.
    Bye {
        reason: Option<String>,
    },
    /// The root ended at its origin: its leases on objects end.
    RootEnd {
        root_id: String,
    },
    Invalidate {
        pattern: JsonMap,
    },
    Prepare(Prepare),
    Vote(Vote),
    Commit {
        reload: u64,
    },
    /// The parent dropped `code_version`: roots of it may no longer come from there.
    Retire {
        code_version: u64,
    },
    Abort {
        reload: u64,
    },
}

impl Frame {
    /// The `t` field.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Hello(_) => "hello",
            Self::Ack { .. } => "ack",
            Self::Hold(_) => "hold",
            Self::Release { .. } => "release",
            Self::Shape(_) => "shape",
            Self::Gone { .. } => "gone",
            Self::Call(_) => "call",
            Self::Result(_) => "result",
            Self::Cancel { .. } => "cancel",
            Self::Sub { .. } => "sub",
            Self::Unsub { .. } => "unsub",
            Self::Resync { .. } => "resync",
            Self::State(_) => "state",
            Self::Ping { .. } => "ping",
            Self::Pong { .. } => "pong",
            Self::Bye { .. } => "bye",
            Self::RootEnd { .. } => "root_end",
            Self::Invalidate { .. } => "invalidate",
            Self::Prepare(_) => "prepare",
            Self::Vote(_) => "vote",
            Self::Commit { .. } => "commit",
            Self::Retire { .. } => "retire",
            Self::Abort { .. } => "abort",
        }
    }
}

/// The wire form of an error: a coded error keeps its code and reason, anything else is `unavailable`.
pub fn to_wire_error(error: &ChordError) -> WireError {
    match error {
        ChordError::Coded { code, reason, message, .. } => {
            WireError { code: *code, message: message.to_string(), reason: reason.as_deref().map(String::from) }
        }
        other => WireError { code: ChordErrorCode::Unavailable, message: other.to_string(), reason: None },
    }
}

/// A coded error from its wire form. The message's `code/reason: ` prefix is stripped, then rebuilt from the fields.
pub fn from_wire_error(error: &WireError) -> ChordError {
    let options = ErrorOptions { reason: error.reason.clone(), ..ErrorOptions::default() };
    ChordError::coded(error.code, strip_code_prefix(&error.message), options)
}

/// The message without a leading `^[a-z_]+(\/[a-z_]+)?: `.
fn strip_code_prefix(message: &str) -> &str {
    let word = |text: &str| text.bytes().take_while(|byte| byte.is_ascii_lowercase() || *byte == b'_').count();
    let head = word(message);
    if head == 0 {
        return message;
    }
    let mut rest = &message[head..];
    if let Some(after) = rest.strip_prefix('/') {
        let tail = word(after);
        if tail > 0 {
            rest = &after[tail..];
        }
    }
    rest.strip_prefix(": ").unwrap_or(message)
}
