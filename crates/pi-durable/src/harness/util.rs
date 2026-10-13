//! Shared helpers of the Harness (upstream `harness/util.ts`).

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::Mutex;

use pi_chord::{ChordError, Context};
use tokio::sync::oneshot;

use crate::errors::{DurableError, DurableResult};
use crate::session::Session;
use crate::types::{Cursor, Page};

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run `future` until it settles or `context` is cancelled, whichever comes first.
pub async fn with_context<T>(future: impl Future<Output = DurableResult<T>>, context: &Context) -> DurableResult<T> {
    let mapped = async move { future.await.map_err(ChordError::from) };
    pi_chord::await_with_context(mapped, context).await.map_err(DurableError::from)
}

/// The cancellation error of a cancelled context: its signal's reason.
pub fn cancellation(context: &Context) -> DurableError {
    context.abort_signal().and_then(|signal| signal.reason()).map_or_else(DurableError::aborted, DurableError::from)
}

/// Pending waits by key. Each settles once: through `resolve`, `reject_all`, or cancellation of its context.
pub struct Waiters<K, T> {
    sets: Mutex<HashMap<K, Vec<oneshot::Sender<DurableResult<T>>>>>,
}

impl<K, T> Default for Waiters<K, T> {
    fn default() -> Self {
        Self { sets: Mutex::new(HashMap::new()) }
    }
}

impl<K: Eq + Hash + Clone, T: Clone + Send + 'static> Waiters<K, T> {
    /// Register a wait; it fails at once when `context` is already cancelled.
    pub fn add(&self, key: K, context: &Context) -> impl Future<Output = DurableResult<T>> + Send + use<K, T> {
        let (sender, receiver) = oneshot::channel();
        let cancelled = context.is_cancelled();
        if !cancelled {
            lock(&self.sets).entry(key).or_default().push(sender);
        }
        let context = context.clone();
        async move {
            if cancelled {
                return Err(cancellation(&context));
            }
            let wait = async move { receiver.await.unwrap_or_else(|_| Err(DurableError::error("Wait was dropped"))) };
            with_context(wait, &context).await
        }
    }

    pub fn keys(&self) -> Vec<K> {
        lock(&self.sets).keys().cloned().collect()
    }

    pub fn resolve(&self, key: &K, value: T) {
        let senders = lock(&self.sets).remove(key).unwrap_or_default();
        for sender in senders {
            let _ = sender.send(Ok(value.clone()));
        }
    }

    pub fn reject_all(&self, error: DurableError) {
        let sets: Vec<_> = lock(&self.sets).drain().collect();
        for (_, senders) in sets {
            for sender in senders {
                let _ = sender.send(Err(error.clone()));
            }
        }
    }
}

/// Every item of a paginated scan, in page order.
pub async fn scan_all<T, F, Fut>(mut scan: F) -> DurableResult<Vec<T>>
where
    F: FnMut(Option<Cursor>) -> Fut,
    Fut: Future<Output = DurableResult<Page<T>>>,
{
    let mut items = Vec::new();
    let mut cursor = None;
    loop {
        let page = scan(cursor).await?;
        items.extend(page.items);
        cursor = page.next;
        if cursor.is_none() {
            return Ok(items);
        }
    }
}

/// What a closed Harness fails with: `SessionFailed` with the cause when the Session failed.
pub fn closed_error(session: &Session) -> DurableError {
    match session.failure() {
        Some(cause) => DurableError::session_failed(cause),
        None => DurableError::error("Harness is closed"),
    }
}

/// Serde adapter for [`pi_chord::JsonMap`] fields: an object, through [`Json`].
pub(crate) mod json_map {
    use pi_chord::{Json, JsonMap};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(map: &JsonMap, serializer: S) -> Result<S::Ok, S::Error> {
        Json::object(map.clone()).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<JsonMap, D::Error> {
        match Json::deserialize(deserializer)? {
            Json::Object(map) => Ok((*map).clone()),
            _ => Err(serde::de::Error::custom("expected an object")),
        }
    }

    pub mod option {
        use super::*;

        pub fn serialize<S: Serializer>(map: &Option<JsonMap>, serializer: S) -> Result<S::Ok, S::Error> {
            map.clone().map(Json::object).serialize(serializer)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<JsonMap>, D::Error> {
            match Option::<Json>::deserialize(deserializer)? {
                None | Some(Json::Null) => Ok(None),
                Some(Json::Object(map)) => Ok(Some((*map).clone())),
                Some(_) => Err(serde::de::Error::custom("expected an object")),
            }
        }
    }
}
