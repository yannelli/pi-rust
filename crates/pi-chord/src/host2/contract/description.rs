//! A contract's description: the data that travels as a reference's contract token (`hold.token`).
//! It keeps the JSON it was built from or received, so relays forward it and fingerprint it unchanged.

use std::sync::Arc;

use indexmap::IndexMap;

use super::Placement;
use crate::host2::schema::{canonical_json, hash64};
use crate::json::{Json, JsonMap};

/// Local contracts have one unnamed variant.
pub const LOCAL_VARIANT: &str = "";

/// Whether objects of a type are addressable from other hosts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Remote,
    Local,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Local => "local",
        }
    }
}

/// One member of a variant. Absent `args`: the method takes none. Absent `result`: it returns nothing.
#[derive(Clone, Debug, PartialEq)]
pub enum MemberDescription {
    Method { args: Option<Json>, result: Option<Json> },
    State { schema: Json },
    Opaque { id: String },
}

impl MemberDescription {
    /// The `kind` field: `method`, `state` or `opaque`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Method { .. } => "method",
            Self::State { .. } => "state",
            Self::Opaque { .. } => "opaque",
        }
    }

    pub(crate) fn to_json(&self) -> Json {
        let mut map = JsonMap::new();
        map.insert("kind", Json::from(self.kind()));
        match self {
            Self::Method { args, result } => {
                if let Some(args) = args {
                    map.insert("args", args.clone());
                }
                if let Some(result) = result {
                    map.insert("result", result.clone());
                }
            }
            Self::State { schema } => {
                map.insert("schema", schema.clone());
            }
            Self::Opaque { id } => {
                map.insert("id", Json::from(id.as_str()));
            }
        }
        Json::object(map)
    }

    fn parse(value: &Json) -> Result<Self, String> {
        let schema = |key: &str, required: bool| match value.get(key) {
            Some(schema @ Json::Object(_)) => Ok(Some(schema.clone())),
            None if !required => Ok(None),
            _ => Err(format!("member {key} is not a schema object")),
        };
        match value.get("kind").and_then(Json::as_str) {
            Some("method") => Ok(Self::Method { args: schema("args", false)?, result: schema("result", false)? }),
            Some("state") => Ok(Self::State { schema: schema("schema", true)?.unwrap_or_default() }),
            Some("opaque") => match value.get("id") {
                Some(Json::String(id)) => Ok(Self::Opaque { id: id.to_string() }),
                _ => Err("opaque member without id".into()),
            },
            _ => Err("member kind is not method, state or opaque".into()),
        }
    }
}

/// Members of one variant, in JavaScript property order.
pub type Members = IndexMap<String, MemberDescription>;

/// The description of a contract (upstream `Description`), validated like upstream's `DescriptionSchema`.
#[derive(Clone, Debug)]
pub struct Description(Arc<Parsed>);

#[derive(Debug)]
struct Parsed {
    raw: Json,
    name: String,
    scope: Scope,
    version: i64,
    input: Json,
    host: Option<Placement>,
    variants: IndexMap<String, Members>,
}

/// The largest integer JavaScript represents exactly.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// A JSON number that is an integer JavaScript represents exactly.
pub(crate) fn safe_integer(value: &Json) -> Option<i64> {
    let number = value.as_f64()?;
    (number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER).then_some(number as i64)
}

fn non_empty(value: Option<&Json>) -> Option<&str> {
    value.and_then(Json::as_str).filter(|text| !text.is_empty())
}

fn parse_placement(value: &Json) -> Result<Placement, String> {
    let map = value.as_object().ok_or("host is not an object")?;
    let only = |keys: &[&str]| map.len() == keys.len() && keys.iter().all(|key| map.contains_key(key));
    if only(&["name"])
        && let Some(name) = non_empty(map.get("name"))
    {
        return Ok(Placement::Named { name: name.into() });
    }
    if only(&["prefix", "key"])
        && let (Some(prefix), Some(key)) = (non_empty(map.get("prefix")), non_empty(map.get("key")))
    {
        return Ok(Placement::Family { prefix: prefix.into(), key: key.into() });
    }
    Err("host is neither { name } nor { prefix, key }".into())
}

impl Description {
    /// Validates a description received as JSON. Unknown fields are kept (and fingerprinted).
    pub fn from_json(raw: Json) -> Result<Self, String> {
        let map = raw.as_object().ok_or("description is not an object")?;
        let name = non_empty(map.get("name")).ok_or("description name is not a non-empty string")?.to_string();
        let scope = match map.get("scope").and_then(Json::as_str) {
            Some("remote") => Scope::Remote,
            Some("local") => Scope::Local,
            _ => return Err("description scope is not remote or local".into()),
        };
        let version = map.get("version").and_then(safe_integer).ok_or("description version is not an integer")?;
        let input = match map.get("input") {
            Some(input @ Json::Object(_)) => input.clone(),
            _ => return Err("description input is not a schema object".into()),
        };
        let host = map.get("host").map(parse_placement).transpose()?;
        let mut variants = IndexMap::new();
        let listed = map.get("variants").and_then(Json::as_object).ok_or("description variants is not an object")?;
        for (kind, members) in listed.iter() {
            let members = members.as_object().ok_or("variant is not an object")?;
            let mut parsed = Members::new();
            for (member, value) in members.iter() {
                parsed.insert(member.to_string(), MemberDescription::parse(value)?);
            }
            variants.insert(kind.to_string(), parsed);
        }
        Ok(Self(Arc::new(Parsed { raw, name, scope, version, input, host, variants })))
    }

    /// The description as JSON, in the order it was built or received.
    pub fn to_json(&self) -> &Json {
        &self.0.raw
    }

    /// Hash of the canonical description.
    pub fn fingerprint(&self) -> String {
        hash64(&canonical_json(&self.0.raw))
    }

    pub fn name(&self) -> &str {
        &self.0.name
    }

    pub fn scope(&self) -> Scope {
        self.0.scope
    }

    pub fn version(&self) -> i64 {
        self.0.version
    }

    pub fn input(&self) -> &Json {
        &self.0.input
    }

    /// Where objects of a remote type live; `None` for local types.
    pub fn host(&self) -> Option<&Placement> {
        self.0.host.as_ref()
    }

    /// Variants and their members, in JavaScript property order.
    pub fn variants(&self) -> &IndexMap<String, Members> {
        &self.0.variants
    }

    pub fn variant(&self, kind: &str) -> Option<&Members> {
        self.0.variants.get(kind)
    }

    /// The placement as JSON (`null` for local types), as canonical comparisons see it.
    pub(crate) fn host_json(&self) -> Json {
        self.0.raw.get("host").cloned().unwrap_or_default()
    }
}

impl PartialEq for Description {
    fn eq(&self, other: &Self) -> bool {
        self.0.raw == other.0.raw
    }
}
