//! Ending objects in bulk: invalidating an identity and closing the host, holders before what they hold.

use std::sync::Arc;

use futures::future::join_all;

use super::{Host, HostImpl};
use crate::error::{ChordError, ChordResult};
use crate::host2::connection::Msg;
use crate::host2::errors::ChordErrorCode;
use crate::host2::object::ObjectImpl;
use crate::host2::schema::deep_equal;
use crate::json::Json;
use crate::sync::BoxFuture;

/// What `invalidate_principal` matches: a predicate, or a pattern of fields that must be equal.
#[derive(Clone)]
pub enum PrincipalMatch {
    Predicate(Arc<dyn Fn(&Json) -> bool + Send + Sync>),
    /// An object of fields; the identity must be an object with equal (deep) values for each.
    Pattern(Json),
}

impl PrincipalMatch {
    pub fn predicate(predicate: impl Fn(&Json) -> bool + Send + Sync + 'static) -> Self {
        Self::Predicate(Arc::new(predicate))
    }

    pub(crate) fn matches(&self, identity: &Json) -> bool {
        match self {
            Self::Predicate(predicate) => predicate(identity),
            Self::Pattern(pattern) => {
                let (Some(pattern), Some(identity)) = (pattern.as_object(), identity.as_object()) else { return false };
                pattern.iter().all(|(key, value)| identity.get(key).is_some_and(|field| deep_equal(field, value)))
            }
        }
    }
}

/// Force-disposes `objects` in waves: an object goes once no object of the set that holds it is left, so
/// holders are disposed while what they hold is still live. Errors of every disposal are collected.
async fn dispose_holders_first(objects: Vec<Arc<ObjectImpl>>, reason: ChordError, message: &str) -> ChordResult<()> {
    let held: Vec<(Arc<ObjectImpl>, Vec<Arc<ObjectImpl>>)> =
        objects.into_iter().map(|object| (object.clone(), object.held_objects())).collect();
    let mut remaining: Vec<usize> = (0..held.len()).collect();
    let mut errors = Vec::new();
    while !remaining.is_empty() {
        let held_by_remaining = |object: &Arc<ObjectImpl>| {
            remaining.iter().any(|index| held[*index].1.iter().any(|target| Arc::ptr_eq(target, object)))
        };
        let mut wave: Vec<usize> =
            remaining.iter().copied().filter(|index| !held_by_remaining(&held[*index].0)).collect();
        // A cycle cannot happen in-host; dispose the rest together rather than loop.
        if wave.is_empty() {
            wave = remaining.clone();
        }
        remaining.retain(|index| !wave.contains(index));
        let disposals = wave.iter().map(|index| held[*index].0.dispose(true, Some(reason.clone())));
        errors.extend(join_all(disposals).await.into_iter().filter_map(Result::err));
    }
    ChordError::collect(errors, message)
}

impl HostImpl {
    fn invalidate_principal(&self, filter: PrincipalMatch) -> BoxFuture<ChordResult<()>> {
        let reason = ChordError::code_only(ChordErrorCode::Gone, "principal invalidated");
        // Links of a matching identity are restamped or closed (with their relays), and patterns reach child hosts.
        let links = self.ask_net(|done| Msg::Invalidate { filter: filter.clone(), done });
        let objects: Vec<Arc<ObjectImpl>> = self.state().objects.values().cloned().collect();
        let matching = objects
            .into_iter()
            .filter(|object| !self.is_self(object.principal()) && filter.matches(object.principal()))
            .collect();
        Box::pin(async move {
            links.await;
            dispose_holders_first(matching, reason, "invalidatePrincipal: disposal failed").await
        })
    }

    /// `invalidate_principal`, for a pattern a parent sent.
    pub(crate) fn invalidate(&self, filter: PrincipalMatch) -> BoxFuture<ChordResult<()>> {
        self.invalidate_principal(filter)
    }

    fn close(&self) -> BoxFuture<ChordResult<()>> {
        for binding in self.lingering() {
            binding.close();
        }
        let names: Vec<String> = {
            let mut state = self.state();
            state.host_objects.clear();
            state.facet_runs.keys().cloned().collect()
        };
        for name in names {
            self.facet_run_changed(&name, None);
        }
        let reason = ChordError::code_only(ChordErrorCode::Gone, format!("host {} closed", self.name));
        let objects: Vec<Arc<ObjectImpl>> = self.state().objects.values().cloned().collect();
        let disposed = dispose_holders_first(objects, reason, "host close: disposal failed");
        // Links close once every object is disposed (a resource's close may still use remote references).
        let me = self.arc();
        Box::pin(async move {
            let result = disposed.await;
            me.ask_net(|done| Msg::CloseAll { reason: "host closed".into(), done }).await;
            result
        })
    }
}

impl Host {
    /// Force-disposes every object whose identity matches (never the host's own).
    pub fn invalidate_principal(&self, filter: PrincipalMatch) -> BoxFuture<ChordResult<()>> {
        self.0.invalidate_principal(filter)
    }

    /// Disposes every object, holders before what they hold.
    pub fn close(&self) -> BoxFuture<ChordResult<()>> {
        self.0.close()
    }
}
