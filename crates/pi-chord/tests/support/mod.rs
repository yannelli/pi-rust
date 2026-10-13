//! Shared helpers for the integration tests: gates, recorders, and polling waits.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::ChordError;
use tokio::sync::watch;

/// A deferred outcome that async listeners can await, like a JavaScript deferred promise.
#[derive(Clone)]
pub struct Gate {
    sender: Arc<watch::Sender<Option<Result<(), String>>>>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub fn new() -> Self {
        Self { sender: Arc::new(watch::channel(None).0) }
    }

    pub fn resolve(&self) {
        self.sender.send_replace(Some(Ok(())));
    }

    pub fn reject(&self, message: &str) {
        self.sender.send_replace(Some(Err(message.to_string())));
    }

    pub async fn wait(&self) -> Result<(), ChordError> {
        let mut receiver = self.sender.subscribe();
        loop {
            if let Some(outcome) = receiver.borrow_and_update().clone() {
                return outcome.map_err(ChordError::Error);
            }
            if receiver.changed().await.is_err() {
                return Err(ChordError::error("gate dropped"));
            }
        }
    }
}

/// A shared, cloneable list.
pub struct Recorder<T>(Arc<Mutex<Vec<T>>>);

impl<T> Clone for Recorder<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Default for Recorder<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
}

impl<T: Clone> Recorder<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, value: T) {
        self.0.lock().unwrap().push(value);
    }

    pub fn get(&self) -> Vec<T> {
        self.0.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

/// Poll until `condition` holds, yielding to spawned tasks between checks.
pub async fn wait_for(mut condition: impl FnMut() -> bool) {
    for _ in 0..2_000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(condition(), "condition not met in time");
}

/// Let spawned tasks run for a few scheduler turns.
pub async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

/// Vitest `toMatchObject`: objects match on the expected keys, arrays element by element.
pub fn matches_object(actual: &pi_chord::Json, expected: &pi_chord::Json) -> bool {
    use pi_chord::Json;
    match (actual, expected) {
        (Json::Object(actual), Json::Object(expected)) => expected
            .iter()
            .all(|(key, value)| actual.get(key.as_ref()).is_some_and(|found| matches_object(found, value))),
        (Json::Array(actual), Json::Array(expected)) => {
            actual.len() == expected.len() && actual.iter().zip(expected.iter()).all(|(a, e)| matches_object(a, e))
        }
        _ => actual == expected,
    }
}
