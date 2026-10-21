//! Typed document tokens and the `define_doc` and `define_doc_family` entry points.

use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use pi_chord::delta::Op;
use pi_chord::{CopyJsonOptions, Json, copy_json};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{CheckpointFn, CheckpointInfo, DocDefinition, DocScope, ResolvedAddress};
use crate::errors::DurableResult;

/// Bound of the typed value of a document or seed. Use `Json` for untyped documents.
pub trait DocValue: Serialize + DeserializeOwned + Send + Sync + 'static {}
impl<T: Serialize + DeserializeOwned + Send + Sync + 'static> DocValue for T {}

/// Typed token of one singleton document definition.
pub struct DocToken<T, S> {
    definition: Arc<DocDefinition>,
    _marker: PhantomData<fn() -> (T, S)>,
}

/// Typed token of one document family definition with creation seed type `I`.
pub struct DocFamilyToken<T, I, S> {
    definition: Arc<DocDefinition>,
    _marker: FamilyMarker<T, I, S>,
}

impl<T, S> Clone for DocToken<T, S> {
    fn clone(&self) -> Self {
        Self { definition: self.definition.clone(), _marker: PhantomData }
    }
}

impl<T, I, S> Clone for DocFamilyToken<T, I, S> {
    fn clone(&self) -> Self {
        Self { definition: self.definition.clone(), _marker: PhantomData }
    }
}

impl<T, S> fmt::Debug for DocToken<T, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("DocToken").field(&self.definition).finish()
    }
}

impl<T, I, S> fmt::Debug for DocFamilyToken<T, I, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("DocFamilyToken").field(&self.definition).finish()
    }
}

impl<T: DocValue, S: DocScope> DocToken<T, S> {
    /// Wrap an erased singleton definition. The caller vouches that `T` and `S` match it.
    pub fn from_definition(definition: Arc<DocDefinition>) -> Self {
        Self { definition, _marker: PhantomData }
    }

    pub fn definition(&self) -> &Arc<DocDefinition> {
        &self.definition
    }

    /// Resolve the address of this singleton for `owner`.
    pub fn resolve(&self, owner: S::Owner) -> ResolvedAddress {
        self.definition.resolve(S::scope_of(owner), None)
    }

    /// Deserialize a snapshot or draft value of this document.
    pub fn decode(&self, value: &Json) -> DurableResult<T> {
        Ok(value.deserialize_into::<T>()?)
    }
}

impl<T: DocValue, I: DocValue, S: DocScope> DocFamilyToken<T, I, S> {
    /// Wrap an erased family definition. The caller vouches that `T`, `I`, and `S` match it.
    pub fn from_definition(definition: Arc<DocDefinition>) -> Self {
        Self { definition, _marker: PhantomData }
    }

    pub fn definition(&self) -> &Arc<DocDefinition> {
        &self.definition
    }

    /// Resolve the address of the member `key` for `owner`.
    pub fn resolve(&self, owner: S::Owner, key: impl Into<String>) -> ResolvedAddress {
        self.definition.resolve(S::scope_of(owner), Some(key.into()))
    }

    pub fn decode(&self, value: &Json) -> DurableResult<T> {
        Ok(value.deserialize_into::<T>()?)
    }
}

type FamilyMarker<T, I, S> = PhantomData<fn() -> (T, I, S)>;
type TypedMigrate<T> = Arc<dyn Fn(&Json, u32) -> DurableResult<T> + Send + Sync>;

/// Typed singleton definition passed to [`define_doc`].
pub struct DocSpec<T, S> {
    kind: String,
    version: u32,
    scope: S,
    initial: Arc<dyn Fn() -> T + Send + Sync>,
    migrate: Option<TypedMigrate<T>>,
    checkpoint_when: CheckpointFn,
}

impl<T: DocValue, S: DocScope> DocSpec<T, S> {
    /// `checkpoint_when` is required and has no default: `|_, _, _| Ok(false)` keeps ordinary changes as deltas, and
    /// returning `Ok(true)` stores the change as a complete base (spec section 3.5).
    ///
    /// ```
    /// # use pi_durable::documents::{DocSpec, SessionScope};
    /// DocSpec::new("app.doc", 1, SessionScope, pi_chord::Json::default, |_, _, _| Ok(false));
    /// ```
    ///
    /// Leaving it out does not compile:
    ///
    /// ```compile_fail
    /// # use pi_durable::documents::{DocSpec, SessionScope};
    /// DocSpec::new("app.doc", 1, SessionScope, pi_chord::Json::default);
    /// ```
    pub fn new(
        kind: impl Into<String>,
        version: u32,
        scope: S,
        initial: impl Fn() -> T + Send + Sync + 'static,
        checkpoint_when: impl Fn(&Json, &[Op], CheckpointInfo) -> DurableResult<bool> + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: kind.into(),
            version,
            scope,
            initial: Arc::new(initial),
            migrate: None,
            checkpoint_when: Arc::new(checkpoint_when),
        }
    }

    /// One callback migrates every supported older stored version to a complete current value.
    pub fn migrate(mut self, migrate: impl Fn(&Json, u32) -> DurableResult<T> + Send + Sync + 'static) -> Self {
        self.migrate = Some(Arc::new(migrate));
        self
    }
}

/// Typed family definition passed to [`define_doc_family`].
pub struct DocFamilySpec<T, I, S> {
    kind: String,
    version: u32,
    scope: S,
    initial: Arc<dyn Fn(I) -> T + Send + Sync>,
    migrate: Option<TypedMigrate<T>>,
    checkpoint_when: CheckpointFn,
}

impl<T: DocValue, I: DocValue, S: DocScope> DocFamilySpec<T, I, S> {
    /// `checkpoint_when` is required and has no default; see [`DocSpec::new`].
    ///
    /// ```compile_fail
    /// # use pi_durable::documents::{DocFamilySpec, SessionScope};
    /// DocFamilySpec::new("app.family", 1, SessionScope, |seed: pi_chord::Json| seed);
    /// ```
    pub fn new(
        kind: impl Into<String>,
        version: u32,
        scope: S,
        initial: impl Fn(I) -> T + Send + Sync + 'static,
        checkpoint_when: impl Fn(&Json, &[Op], CheckpointInfo) -> DurableResult<bool> + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: kind.into(),
            version,
            scope,
            initial: Arc::new(initial),
            migrate: None,
            checkpoint_when: Arc::new(checkpoint_when),
        }
    }

    pub fn migrate(mut self, migrate: impl Fn(&Json, u32) -> DurableResult<T> + Send + Sync + 'static) -> Self {
        self.migrate = Some(Arc::new(migrate));
        self
    }
}

/// Serialize a typed value into strict JSON; absent optional properties are omitted.
pub(crate) fn encode<T: Serialize + ?Sized>(value: &T) -> DurableResult<Json> {
    Ok(copy_json(value, CopyJsonOptions { omit_undefined_properties: true })?)
}

fn finish<T: DocValue>(mut definition: DocDefinition, migrate: Option<TypedMigrate<T>>) -> DocDefinition {
    if let Some(migrate) = migrate {
        definition = definition.with_migrate(move |value, from| encode(&migrate(value, from)?));
    }
    definition
}

/// Define a singleton document. The scope marker fixes the owner type of every access.
pub fn define_doc<T: DocValue, S: DocScope>(spec: DocSpec<T, S>) -> DurableResult<DocToken<T, S>> {
    let initial = spec.initial;
    let semantics = spec.scope.semantics();
    let definition = DocDefinition::from_shared(
        spec.kind,
        spec.version,
        semantics,
        false,
        Arc::new(move |_| encode(&initial())),
        spec.checkpoint_when,
    )?;
    let definition = finish(definition, spec.migrate);
    Ok(DocToken::from_definition(Arc::new(definition)))
}

/// Define a document family whose members are created from a typed seed.
pub fn define_doc_family<T: DocValue, I: DocValue, S: DocScope>(
    spec: DocFamilySpec<T, I, S>,
) -> DurableResult<DocFamilyToken<T, I, S>> {
    let initial = spec.initial;
    let semantics = spec.scope.semantics();
    let definition = DocDefinition::from_shared(
        spec.kind,
        spec.version,
        semantics,
        true,
        Arc::new(move |seed| {
            let seed = seed.cloned().unwrap_or(Json::Null).deserialize_into::<I>()?;
            encode(&initial(seed))
        }),
        spec.checkpoint_when,
    )?;
    let definition = finish(definition, spec.migrate);
    Ok(DocFamilyToken::from_definition(Arc::new(definition)))
}
