//! Document incarnation records and the stored content of documents (spec sections 3 and 10).

use pi_chord::Json;
use pi_chord::delta::Op;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ids::{ConversationId, DocumentId, Seq, TaskId};

/// Owner of a document incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DocumentScope {
    Session,
    #[serde(rename_all = "camelCase")]
    Conversation {
        conversation_id: ConversationId,
    },
    #[serde(rename_all = "camelCase")]
    Task {
        task_id: TaskId,
    },
}

impl DocumentScope {
    /// `"session"`, `"conversation"`, or `"task"`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Conversation { .. } => "conversation",
            Self::Task { .. } => "task",
        }
    }

    /// The owning conversation or task ID; `None` for the Session.
    pub fn owner_id(&self) -> Option<u64> {
        match self {
            Self::Session => None,
            Self::Conversation { conversation_id } => Some(conversation_id.0),
            Self::Task { task_id } => Some(task_id.0),
        }
    }
}

/// How much history a conversation document keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DocumentHistory {
    /// Keep only the current state.
    Latest,
    /// Keep history needed for as-of reads.
    Rewindable,
}

/// What a fork of the owning conversation starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DocumentFork {
    /// The value at the fork cutoff; rewindable documents only.
    AsOf,
    Current,
    Initial,
}

/// Fields supplied when storage creates and stamps a new `DocumentRecord`. `history` and `fork` are present exactly
/// for conversation documents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentCreate {
    /// Unique incarnation ID; never reused when the same logical document is recreated.
    pub id: DocumentId,
    /// Stable document definition kind.
    pub kind: String,
    /// Family member key; absent for singleton documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub scope: DocumentScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<DocumentHistory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<DocumentFork>,
}

/// Persisted lifecycle record for one create-to-retire document incarnation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentRecord {
    pub id: DocumentId,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub scope: DocumentScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<DocumentHistory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<DocumentFork>,
    /// Commit that created the incarnation, stamped by storage.
    pub created_at: Seq,
    /// Commit that retired the incarnation; absent while it is current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<Seq>,
}

impl DocumentCreate {
    /// Stamp the creating commit.
    pub fn into_record(self, created_at: Seq) -> DocumentRecord {
        DocumentRecord {
            id: self.id,
            kind: self.kind,
            key: self.key,
            scope: self.scope,
            history: self.history,
            fork: self.fork,
            created_at,
            retired_at: None,
        }
    }

    pub fn address(&self) -> DocumentAddress {
        DocumentAddress { kind: self.kind.clone(), scope: self.scope, key: self.key.clone() }
    }
}

impl DocumentRecord {
    /// The create fields, without lifetime stamps.
    pub fn to_create(&self) -> DocumentCreate {
        DocumentCreate {
            id: self.id,
            kind: self.kind.clone(),
            key: self.key.clone(),
            scope: self.scope,
            history: self.history,
            fork: self.fork,
        }
    }

    pub fn address(&self) -> DocumentAddress {
        DocumentAddress { kind: self.kind.clone(), scope: self.scope, key: self.key.clone() }
    }
}

/// Current state or one historical commit sequence used for document membership and content reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DocumentPoint {
    Current,
    At(Seq),
}

impl Serialize for DocumentPoint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Current => serializer.serialize_str("current"),
            Self::At(seq) => serializer.serialize_u64(seq.0),
        }
    }
}

impl<'de> Deserialize<'de> for DocumentPoint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Json::deserialize(deserializer)? {
            Json::String(text) if &*text == "current" => Ok(Self::Current),
            Json::Number(number) => number
                .as_u64()
                .map(|value| Self::At(Seq(value)))
                .ok_or_else(|| serde::de::Error::custom("document point must be a non-negative integer")),
            _ => Err(serde::de::Error::custom("document point must be \"current\" or a sequence")),
        }
    }
}

/// Exact logical identity of a singleton or one keyed family member.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentAddress {
    pub kind: String,
    pub scope: DocumentScope,
    /// Absent selects the singleton; present selects one family member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Ordered scan of document incarnations alive in one exact scope at one point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentQuery {
    pub scope: DocumentScope,
    pub at: DocumentPoint,
    pub kind: Option<String>,
}

/// Complete checkpoint or Chord operation batch selected by the owning Session. Serialized as upstream builds it:
/// `{"version":1,"kind":"base","value":{}}` or `{"version":1,"kind":"delta","ops":[]}`.
#[derive(Clone, Debug, PartialEq)]
pub enum DocumentContent {
    Base { version: u32, value: Json },
    Delta { version: u32, ops: Vec<Op> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContentOut<'a> {
    version: u32,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'a Json>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ops: Option<&'a [Op]>,
}

#[derive(Deserialize)]
struct ContentIn {
    version: u32,
    kind: String,
    #[serde(default)]
    value: Option<Json>,
    #[serde(default)]
    ops: Option<Vec<Op>>,
}

impl Serialize for DocumentContent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let out = match self {
            Self::Base { version, value } => {
                ContentOut { version: *version, kind: "base", value: Some(value), ops: None }
            }
            Self::Delta { version, ops } => {
                ContentOut { version: *version, kind: "delta", value: None, ops: Some(ops) }
            }
        };
        out.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DocumentContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let content = ContentIn::deserialize(deserializer)?;
        match (content.kind.as_str(), content.value, content.ops) {
            ("base", Some(value), _) => Ok(Self::Base { version: content.version, value }),
            ("delta", _, Some(ops)) => Ok(Self::Delta { version: content.version, ops }),
            (kind, ..) => Err(serde::de::Error::custom(format!("invalid document content kind {kind}"))),
        }
    }
}

impl DocumentContent {
    pub fn version(&self) -> u32 {
        match self {
            Self::Base { version, .. } | Self::Delta { version, .. } => *version,
        }
    }
}

/// Exact persisted source selected for a definition-free document copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentCopySource {
    pub id: DocumentId,
    pub at: DocumentPoint,
}

/// Detached materialized value and stored definition version at a selected point.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredDocument {
    pub record: DocumentRecord,
    pub version: u32,
    /// A JSON object.
    pub value: Json,
    /// Deltas replayed after the selected base to materialize `value`.
    pub deltas_since_base: u32,
}
