//! Services (a type's factory and serve options) and implementations: members looked up by name.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;

use super::{Ctx, Declare};
use crate::error::{ChordError, ChordResult};
use crate::host2::contract::{Accepts, Contract, LOCAL_VARIANT, MemberDescription, Members, Scope};
use crate::host2::errors::invalid;
use crate::host2::schema::normalize;
use crate::host2::types::ReadableRef;
use crate::json::Json;
use crate::sync::BoxFuture;

/// A method: its arguments (`null` for a method without any) and the call's context.
pub type MethodFn = Arc<dyn Fn(Json, Ctx) -> BoxFuture<ChordResult<Json>> + Send + Sync>;

/// One member of an implementation.
#[derive(Clone)]
pub enum Member {
    Method(MethodFn),
    /// A cell, a derivation, or a held object's state member.
    State(ReadableRef),
    /// A native value; [`Declare::expose`] makes one backed by a resource.
    Opaque(Arc<dyn Any + Send + Sync>),
}

impl Member {
    pub fn method<F, Fut>(method: F) -> Self
    where
        F: Fn(Json, Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChordResult<Json>> + Send + 'static,
    {
        Self::Method(Arc::new(move |args, ctx| Box::pin(method(args, ctx))))
    }

    pub fn opaque<T: Any + Send + Sync>(value: T) -> Self {
        Self::Opaque(Arc::new(value))
    }
}

/// What a factory returns: the variant (remote types) and the members by name, in declaration order.
#[derive(Clone, Default)]
pub struct Implementation {
    variant: Option<String>,
    members: IndexMap<String, Member>,
}

impl Implementation {
    pub fn new() -> Self {
        Self::default()
    }

    /// An implementation of variant `variant` (remote types).
    pub fn variant(variant: &str) -> Self {
        Self { variant: Some(variant.to_string()), members: IndexMap::new() }
    }

    pub fn member(mut self, name: &str, member: Member) -> Self {
        self.members.insert(name.to_string(), member);
        self
    }

    pub fn method<F, Fut>(self, name: &str, method: F) -> Self
    where
        F: Fn(Json, Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChordResult<Json>> + Send + 'static,
    {
        self.member(name, Member::method(method))
    }

    pub fn state(self, name: &str, readable: ReadableRef) -> Self {
        self.member(name, Member::State(readable))
    }

    pub fn opaque<T: Any + Send + Sync>(self, name: &str, value: T) -> Self {
        self.member(name, Member::opaque(value))
    }

    pub fn variant_name(&self) -> Option<&str> {
        self.variant.as_deref()
    }

    pub fn get(&self, name: &str) -> Option<&Member> {
        self.members.get(name)
    }

    /// Member names, in declaration order (`variant` is not one).
    pub fn names(&self) -> Vec<String> {
        self.members.keys().cloned().collect()
    }

    pub fn members(&self) -> impl Iterator<Item = (&str, &Member)> {
        self.members.iter().map(|(name, member)| (name.as_str(), member))
    }
}

/// A type's factory: the normalized input, the identity the object is for, and what it declares through.
pub type Factory = Arc<dyn Fn(&Json, &Json, &mut Declare) -> ChordResult<Implementation> + Send + Sync>;

/// Maps an implementation to what consumers of an older contract version see.
pub type Adapter = Arc<dyn Fn(&Implementation) -> Implementation + Send + Sync>;

/// How long objects of a type live.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lifetime {
    /// Created at the first `get` of an address, disposed after the last reference is released (plus `linger`).
    #[default]
    Referenced,
    /// One object, created when served and kept until the host closes. Only these may register.
    Host,
}

/// `serve` options.
#[derive(Clone, Default)]
pub struct ServeOptions {
    pub lifetime: Lifetime,
    /// How long an object stays after its last reference is released.
    pub linger: Option<Duration>,
    /// Adapters for consumers built against older contract versions.
    pub accepts: Option<Accepts<Adapter>>,
}

impl ServeOptions {
    /// `lifetime: "host"`.
    pub fn host() -> Self {
        Self { lifetime: Lifetime::Host, ..Self::default() }
    }

    /// `linger` after the last release.
    pub fn linger(linger: Duration) -> Self {
        Self { linger: Some(linger), ..Self::default() }
    }
}

/// A served type: its contract, factory and options, and the facet that served it (empty: `host.serve`).
pub struct Service {
    pub contract: Contract,
    pub factory: Factory,
    pub options: ServeOptions,
    pub facet: String,
}

impl Service {
    pub fn host_lifetime(&self) -> bool {
        self.options.lifetime == Lifetime::Host
    }
}

pub(crate) fn make_service(
    contract: &Contract,
    factory: Factory,
    options: ServeOptions,
    facet: &str,
) -> ChordResult<Arc<Service>> {
    check_lifetime(contract, &options)?;
    Ok(Arc::new(Service { contract: contract.clone(), factory, options, facet: facet.to_string() }))
}

/// Refuses lifetimes the type cannot have, when it is served.
fn check_lifetime(contract: &Contract, options: &ServeOptions) -> ChordResult<()> {
    if options.lifetime != Lifetime::Host {
        return Ok(());
    }
    let name = contract.name();
    if contract.scope() != Scope::Local {
        return Err(ChordError::type_error(format!(
            "{name}: lifetime \"host\" needs a local type (remote types are referenced)"
        )));
    }
    if options.linger.is_some() {
        return Err(ChordError::type_error(format!(
            "{name}: an object of lifetime \"host\" does not linger (it lives as long as the host)"
        )));
    }
    if normalize(contract.input(), Some(&Json::empty_object()), &format!("{name} input")).is_err() {
        return Err(ChordError::type_error(format!(
            "{name}: lifetime \"host\" needs an input that accepts {{}} (the object's one address)"
        )));
    }
    Ok(())
}

/// The remote state-member schemas an implementation exposes, by source id.
pub(crate) fn member_schemas(service: &Service, implementation: &Implementation) -> HashMap<usize, Vec<Json>> {
    let mut schemas: HashMap<usize, Vec<Json>> = HashMap::new();
    let contract = &service.contract;
    if contract.scope() != Scope::Remote {
        return schemas;
    }
    let Some(members) = contract.description().variant(implementation.variant_name().unwrap_or_default()) else {
        return schemas;
    };
    for (name, description) in members {
        let (MemberDescription::State { schema }, Some(Member::State(readable))) =
            (description, implementation.get(name))
        else {
            continue;
        };
        if let Some(core) = readable.source() {
            schemas.entry(core.id()).or_default().push(schema.clone());
        }
    }
    schemas
}

pub(crate) fn validate_implementation(service: &Service, implementation: &Implementation) -> ChordResult<()> {
    let contract = &service.contract;
    let fail = |message: String| invalid("implementation", format!("{}: {message}", contract.name()), None);
    let empty = Members::new();
    let members = if contract.scope() == Scope::Remote {
        let variant = implementation.variant_name();
        let Some(members) = variant.and_then(|variant| contract.description().variant(variant)) else {
            return Err(fail(format!("unknown variant {}", variant.unwrap_or("undefined"))));
        };
        if let Some(name) = implementation.members.keys().find(|name| !members.contains_key(*name)) {
            return Err(fail(format!("{name} is not in variant {}", variant.unwrap_or_default())));
        }
        members
    } else {
        contract.description().variant(LOCAL_VARIANT).unwrap_or(&empty)
    };
    for (name, description) in members {
        let value = implementation.get(name);
        match description {
            MemberDescription::Method { .. } if !matches!(value, Some(Member::Method(_))) => {
                return Err(fail(format!("{name} must be a method")));
            }
            MemberDescription::State { .. } if !matches!(value, Some(Member::State(_))) => {
                return Err(fail(format!("{name} must be a cell, a derivation or a held object's state member")));
            }
            MemberDescription::Opaque { .. } if value.is_none() => return Err(fail(format!("{name} is missing"))),
            _ => {}
        }
    }
    Ok(())
}
