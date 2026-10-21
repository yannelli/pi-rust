//! Port of upstream `contract.ts`: contracts (`remote`, `local`), their descriptions and fingerprints, and how a
//! provider's contract binds a consumer's (`coverage`, `bind`). TypeScript's static `Ref`/`ImplementationOf` types
//! have no runtime part and are left to the object layer.

mod bind;
mod description;

use std::sync::Arc;

pub use bind::{Accepts, bind, coverage, takes_no_args};
pub(crate) use description::safe_integer;
pub use description::{Description, LOCAL_VARIANT, MemberDescription, Members, Scope};

use super::errors::invalid;
use super::schema::{canonical_json, hash64};
use crate::error::{ChordError, ChordResult};
use crate::json::{Json, JsonMap, array_index_key};

/// A method's arguments and result. `None`: it takes no arguments, or returns nothing (void).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MethodSpec {
    pub args: Option<Json>,
    pub result: Option<Json>,
}

/// A method: `method(None, None)` takes no arguments and returns nothing (called as `ref.name(ctx)`).
/// `method(args, None)` returns nothing; `method(None, result)` takes no arguments and returns something.
pub fn method(args: impl Into<Option<Json>>, result: impl Into<Option<Json>>) -> MethodSpec {
    MethodSpec { args: args.into(), result: result.into() }
}

/// A native in-process object identified by `name@version` (local contracts only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpaqueSpec {
    id: String,
}

impl OpaqueSpec {
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// JavaScript's `\s`: Unicode white space without U+0085, plus U+FEFF.
fn js_whitespace(c: char) -> bool {
    c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}')
}

/// An opaque member; `id` must match `^[^@\s]+@\d+$`.
pub fn opaque(id: &str) -> ChordResult<OpaqueSpec> {
    let valid = id.split_once('@').is_some_and(|(name, version)| {
        !name.is_empty()
            && !name.chars().any(js_whitespace)
            && !version.is_empty()
            && version.bytes().all(|byte| byte.is_ascii_digit())
    });
    if !valid {
        return Err(ChordError::type_error(format!("opaque id must be name@version: {id}")));
    }
    Ok(OpaqueSpec { id: id.into() })
}

/// A member as a definition lists it: a method, an opaque native object, or a state member's schema.
#[derive(Clone, Debug, PartialEq)]
pub enum Member {
    Method(MethodSpec),
    Opaque(OpaqueSpec),
    State(Json),
}

impl From<MethodSpec> for Member {
    fn from(spec: MethodSpec) -> Self {
        Self::Method(spec)
    }
}

impl From<OpaqueSpec> for Member {
    fn from(spec: OpaqueSpec) -> Self {
        Self::Opaque(spec)
    }
}

impl From<Json> for Member {
    fn from(schema: Json) -> Self {
        Self::State(schema)
    }
}

/// One variant of a remote type: its state members and its methods, each in JavaScript property order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VariantDef {
    pub state: Vec<(String, Member)>,
    pub methods: Vec<(String, Member)>,
}

impl VariantDef {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(mut self, name: impl Into<String>, member: impl Into<Member>) -> Self {
        self.state.push((name.into(), member.into()));
        self
    }

    pub fn method(mut self, name: impl Into<String>, member: impl Into<Member>) -> Self {
        self.methods.push((name.into(), member.into()));
        self
    }
}

/// Where objects of a remote type live: data only, so a description carries it and every host can check it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// One named host.
    Named { name: String },
    /// The host `prefix` + the input's string field `key` (`session:` + `session`).
    Family { prefix: String, key: String },
}

impl Placement {
    pub(crate) fn to_json(&self) -> Json {
        let mut map = JsonMap::new();
        match self {
            Self::Named { name } => {
                map.insert("name", Json::from(name.as_str()));
            }
            Self::Family { prefix, key } => {
                map.insert("prefix", Json::from(prefix.as_str()));
                map.insert("key", Json::from(key.as_str()));
            }
        }
        Json::object(map)
    }

    fn is_valid(&self) -> bool {
        match self {
            Self::Named { name } => !name.is_empty(),
            Self::Family { prefix, key } => !prefix.is_empty() && !key.is_empty(),
        }
    }
}

/// Objects of the type live on the host named `name`.
pub fn host_named(name: &str) -> ChordResult<Placement> {
    if name.is_empty() {
        return Err(ChordError::type_error("hostNamed needs a host name"));
    }
    Ok(Placement::Named { name: name.into() })
}

/// Objects of the type live on `prefix` + the value of the input's string field `key`: `host_family("session:",
/// "session")` puts `{ session: "S1" }` on `session:S1`. The field must be a non-empty string.
pub fn host_family(prefix: &str, key: &str) -> ChordResult<Placement> {
    if prefix.is_empty() {
        return Err(ChordError::type_error("hostFamily needs a host-name prefix"));
    }
    if key.is_empty() {
        return Err(ChordError::type_error("hostFamily needs an input field name"));
    }
    Ok(Placement::Family { prefix: prefix.into(), key: key.into() })
}

/// The host an object of `placement` with this (normalized) input lives on. Fails with `invalid/placement`.
pub fn resolve_host(type_name: &str, placement: &Placement, input: &Json) -> ChordResult<String> {
    match placement {
        Placement::Named { name } => Ok(name.clone()),
        Placement::Family { prefix, key } => match input.get(key).and_then(Json::as_str) {
            Some(value) if !value.is_empty() => Ok(format!("{prefix}{value}")),
            _ => Err(invalid("placement", format!("{type_name}: input field {key} names no host"), None)),
        },
    }
}

/// Whether the host named `host_name` may serve a type of `placement`.
pub fn placed_on(placement: &Placement, host_name: &str) -> bool {
    match placement {
        Placement::Named { name } => name == host_name,
        Placement::Family { prefix, .. } => host_name.len() > prefix.len() && host_name.starts_with(prefix.as_str()),
    }
}

pub fn placement_text(placement: &Placement) -> String {
    match placement {
        Placement::Named { name } => name.clone(),
        Placement::Family { prefix, key } => format!("{prefix}<{key}>"),
    }
}

/// A remote type: addressable from other hosts, placed by `host`, with one or more variants.
#[derive(Clone, Debug)]
pub struct RemoteDef {
    pub version: i64,
    pub input: Json,
    /// Where its objects live. Part of the contract (and its fingerprint).
    pub host: Placement,
    pub variants: Vec<(String, VariantDef)>,
}

/// A local type: one unnamed variant of members.
#[derive(Clone, Debug)]
pub struct LocalDef {
    pub version: i64,
    pub input: Json,
    pub members: Vec<(String, Member)>,
}

/// Names a reference uses itself; members may not use them.
pub const RESERVED_MEMBERS: [&str; 10] =
    ["variant", "status", "ready", "shape", "error", "onShape", "onStatus", "release", "then", "contract"];

/// A contract: its description and the fingerprint of the canonical description.
#[derive(Clone, Debug)]
pub struct Contract(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    description: Description,
    fingerprint: String,
}

impl Contract {
    fn finish(description: Description) -> Self {
        let fingerprint = hash64(&canonical_json(description.to_json()));
        Self(Arc::new(Inner { description, fingerprint }))
    }

    pub fn name(&self) -> &str {
        self.0.description.name()
    }

    pub fn scope(&self) -> Scope {
        self.0.description.scope()
    }

    pub fn version(&self) -> i64 {
        self.0.description.version()
    }

    pub fn input(&self) -> &Json {
        self.0.description.input()
    }

    pub fn description(&self) -> &Description {
        &self.0.description
    }

    pub fn fingerprint(&self) -> &str {
        &self.0.fingerprint
    }
}

impl PartialEq for Contract {
    fn eq(&self, other: &Self) -> bool {
        self.0.fingerprint == other.0.fingerprint && self.0.description == other.0.description
    }
}

/// Entries in JavaScript property order: array-index names ascending first, then the rest as listed.
fn js_order<T>(entries: &[(String, T)]) -> Vec<&(String, T)> {
    let mut ordered: Vec<_> = entries.iter().collect();
    ordered.sort_by_key(|(name, _)| array_index_key(name).map_or((1, 0), |index| (0, index)));
    ordered
}

fn describe_member(name: &str, member: &Member, scope: Scope) -> ChordResult<MemberDescription> {
    if RESERVED_MEMBERS.contains(&name) {
        return Err(ChordError::type_error(format!("member name is reserved: {name}")));
    }
    Ok(match member {
        Member::Method(spec) => MemberDescription::Method { args: spec.args.clone(), result: spec.result.clone() },
        Member::Opaque(_) if scope == Scope::Remote => {
            return Err(ChordError::type_error(format!("remote contracts cannot expose opaque members: {name}")));
        }
        Member::Opaque(spec) => MemberDescription::Opaque { id: spec.id.clone() },
        Member::State(schema) => MemberDescription::State { schema: schema.clone() },
    })
}

fn check_header(name: &str, version: i64) -> ChordResult<()> {
    if name.is_empty() {
        return Err(ChordError::type_error("contract name is empty"));
    }
    if version < 1 {
        return Err(ChordError::type_error("contract version must be a positive integer"));
    }
    Ok(())
}

/// The description built from a definition; a non-object schema (impossible in TypeScript) is a type error.
fn described(name: &str, raw: JsonMap) -> ChordResult<Contract> {
    let description = Description::from_json(Json::object(raw))
        .map_err(|error| ChordError::type_error(format!("contract {name}: {error}")))?;
    Ok(Contract::finish(description))
}

fn header(name: &str, scope: Scope, version: i64, input: &Json) -> JsonMap {
    let mut raw = JsonMap::new();
    raw.insert("name", Json::from(name));
    raw.insert("scope", Json::from(scope.as_str()));
    raw.insert("version", Json::from(version as f64));
    raw.insert("input", input.clone());
    raw
}

pub fn remote(name: &str, def: &RemoteDef) -> ChordResult<Contract> {
    check_header(name, def.version)?;
    if !def.host.is_valid() {
        // concise-ignore: upstream's message
        let message = format!("remote contract {name} needs `host`: hostNamed(\u{2026}) or hostFamily(\u{2026})");
        return Err(ChordError::type_error(message));
    }
    if def.variants.is_empty() {
        return Err(ChordError::type_error(format!("remote contract {name} has no variants")));
    }
    let mut variants = JsonMap::new();
    for (kind, variant) in js_order(&def.variants) {
        let mut members = JsonMap::new();
        for (key, member) in js_order(&variant.state) {
            members.insert(key.as_str(), describe_member(key, member, Scope::Remote)?.to_json());
        }
        for (key, member) in js_order(&variant.methods) {
            if members.contains_key(key) {
                return Err(ChordError::type_error(format!("member {key} is both state and method in {name}.{kind}")));
            }
            let described = describe_member(key, member, Scope::Remote)?;
            if !matches!(described, MemberDescription::Method { .. }) {
                return Err(ChordError::type_error(format!("methods.{key} is not a method()")));
            }
            members.insert(key.as_str(), described.to_json());
        }
        variants.insert(kind.as_str(), Json::object(members));
    }
    let mut raw = header(name, Scope::Remote, def.version, &def.input);
    raw.insert("host", def.host.to_json());
    raw.insert("variants", Json::object(variants));
    described(name, raw)
}

pub fn local(name: &str, def: &LocalDef) -> ChordResult<Contract> {
    check_header(name, def.version)?;
    let mut members = JsonMap::new();
    for (key, member) in js_order(&def.members) {
        members.insert(key.as_str(), describe_member(key, member, Scope::Local)?.to_json());
    }
    let mut raw = header(name, Scope::Local, def.version, &def.input);
    raw.insert("variants", Json::object(JsonMap::from_iter([(LOCAL_VARIANT, Json::object(members))])));
    described(name, raw)
}

/// A consumer token received over a link (only its description travels): good for binding and validation.
pub fn contract_from_description(description: Description) -> ChordResult<Contract> {
    if (description.scope() == Scope::Remote) != description.host().is_some() {
        return Err(invalid("token", "malformed contract token", None));
    }
    Ok(Contract::finish(description))
}
