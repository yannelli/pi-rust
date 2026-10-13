//! One facet's lifecycle: setup, activation, owned resources, and teardown.

use std::fmt;
use std::sync::Mutex;

use crate::error::{ChordError, ChordResult};
use crate::sync::{BoxFuture, lock};

/// A resource cleanup a facet owns; it runs once, in reverse registration order.
pub type Disposal = Box<dyn FnOnce() -> BoxFuture<ChordResult<()>> + Send>;
/// Starts one keyed observation when the facet activates and returns its cleanup.
pub(crate) type ObservationStart = Box<dyn FnOnce() -> ChordResult<Disposal> + Send>;
/// Asynchronous initialization after dependencies are bound and ready.
pub type ActivateCallback = Box<dyn FnOnce() -> BoxFuture<ChordResult<()>> + Send>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LifecycleState {
    SettingUp,
    Prepared,
    Active,
    Disposing,
    Dead,
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SettingUp => "setting_up",
            Self::Prepared => "prepared",
            Self::Active => "active",
            Self::Disposing => "disposing",
            Self::Dead => "dead",
        })
    }
}

struct Inner {
    state: LifecycleState,
    service_access: bool,
    effects: Vec<Disposal>,
    observations: Vec<ObservationStart>,
    activate: Vec<ActivateCallback>,
}

pub(crate) struct FacetLifecycle {
    pub id: String,
    inner: Mutex<Inner>,
}

impl FacetLifecycle {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            inner: Mutex::new(Inner {
                state: LifecycleState::SettingUp,
                service_access: false,
                effects: Vec::new(),
                observations: Vec::new(),
                activate: Vec::new(),
            }),
        }
    }

    fn state(&self) -> LifecycleState {
        lock(&self.inner).state
    }

    pub fn assert_setting_up(&self, operation: &str) -> ChordResult<()> {
        if self.state() != LifecycleState::SettingUp {
            return Err(ChordError::error(format!("Facet {} can {operation} only during setup", self.id)));
        }
        Ok(())
    }

    pub fn assert_running(&self, operation: &str) -> ChordResult<()> {
        let state = self.state();
        if state != LifecycleState::SettingUp && state != LifecycleState::Active {
            return Err(ChordError::error(format!("Facet {} cannot {operation} while {state}", self.id)));
        }
        Ok(())
    }

    pub fn assert_active(&self, operation: &str) -> ChordResult<()> {
        if self.state() != LifecycleState::Active {
            return Err(ChordError::error(format!("Facet {} can {operation} only while active", self.id)));
        }
        Ok(())
    }

    pub fn assert_service_access(&self) -> ChordResult<()> {
        let inner = lock(&self.inner);
        if !inner.service_access {
            let message = format!("Facet {} service handles cannot be used while {}", self.id, inner.state);
            return Err(ChordError::error(message));
        }
        Ok(())
    }

    pub fn revoke(&self) {
        lock(&self.inner).service_access = false;
    }

    pub fn own(&self, disposal: Disposal) -> ChordResult<()> {
        self.assert_running("own resources")?;
        lock(&self.inner).effects.push(disposal);
        Ok(())
    }

    pub fn observe(&self, start: ObservationStart) -> ChordResult<()> {
        self.assert_setting_up("observe services")?;
        lock(&self.inner).observations.push(start);
        Ok(())
    }

    pub fn on_activate(&self, callback: ActivateCallback) -> ChordResult<()> {
        self.assert_setting_up("register activation callbacks")?;
        lock(&self.inner).activate.push(callback);
        Ok(())
    }

    pub fn prepared(&self) -> ChordResult<()> {
        self.assert_setting_up("finish setup")?;
        lock(&self.inner).state = LifecycleState::Prepared;
        Ok(())
    }

    pub async fn activate(&self) -> ChordResult<()> {
        let (observations, callbacks) = {
            let mut inner = lock(&self.inner);
            if inner.state != LifecycleState::Prepared {
                return Err(ChordError::error(format!("Facet {} is not prepared", self.id)));
            }
            inner.state = LifecycleState::Active;
            inner.service_access = true;
            (std::mem::take(&mut inner.observations), std::mem::take(&mut inner.activate))
        };
        for start in observations {
            let disposal = start()?;
            lock(&self.inner).effects.push(disposal);
        }
        for callback in callbacks {
            callback().await?;
        }
        Ok(())
    }

    pub async fn dispose(&self) -> ChordResult<()> {
        let effects = {
            let mut inner = lock(&self.inner);
            if inner.state == LifecycleState::Dead {
                return Ok(());
            }
            inner.state = LifecycleState::Disposing;
            std::mem::take(&mut inner.effects)
        };
        let mut errors = Vec::new();
        for effect in effects.into_iter().rev() {
            if let Err(error) = effect().await {
                errors.push(error);
            }
        }
        {
            let mut inner = lock(&self.inner);
            inner.observations.clear();
            inner.activate.clear();
            inner.service_access = false;
            inner.state = LifecycleState::Dead;
        }
        single_or_aggregate(errors, &format!("Failed to dispose facet {}", self.id))
    }
}

/// Upstream's rule: one failure is thrown as is, several become an aggregate.
pub(crate) fn single_or_aggregate(mut errors: Vec<ChordError>, message: &str) -> ChordResult<()> {
    match errors.len() {
        0 => Ok(()),
        1 => Err(errors.remove(0)),
        _ => Err(ChordError::Aggregate { message: message.to_string(), errors }),
    }
}
