//! Application-owned registry of extensions (upstream `createRegistry`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::agent::INSTRUCTIONS_KEY;
use super::define::{Extension, PromptSection};
use super::task::Task;
use super::tool_registration::ToolRegistration;
use crate::errors::{DurableError, DurableResult};

/// Built-in task definitions every registry holds; they cannot be removed or replaced.
pub fn builtin_tasks() -> Vec<Task> {
    vec![super::generation::generation_task(), super::tool::tool_task(), super::compaction::compaction_task()]
}

struct RegistryState {
    extensions: Vec<Arc<Extension>>,
    by_name: HashMap<String, Arc<Extension>>,
    tasks: Vec<Task>,
    task_index: HashMap<String, usize>,
}

/// Immutable view of one published registry state. Cheap to clone.
#[derive(Clone)]
pub struct RegistrySnapshot(Arc<RegistryState>);

impl RegistrySnapshot {
    fn build(extensions: Vec<Arc<Extension>>) -> DurableResult<Self> {
        let mut tasks: Vec<Task> = Vec::new();
        let mut task_index = HashMap::new();
        for task in builtin_tasks() {
            task_index.insert(task.name.clone(), tasks.len());
            tasks.push(task);
        }
        for extension in &extensions {
            for task in &extension.tasks {
                if task_index.contains_key(&task.name) {
                    let message = format!("Task {} of extension {} is already installed", task.name, extension.name);
                    return Err(DurableError::error(message));
                }
                task_index.insert(task.name.clone(), tasks.len());
                tasks.push(task.clone());
            }
        }
        let by_name = extensions.iter().map(|extension| (extension.name.clone(), extension.clone())).collect();
        Ok(Self(Arc::new(RegistryState { extensions, by_name, tasks, task_index })))
    }

    /// The same published state.
    pub fn ptr_eq(&self, other: &RegistrySnapshot) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn installed(&self) -> &[Arc<Extension>] {
        &self.0.extensions
    }

    pub fn extension(&self, name: &str) -> Option<Arc<Extension>> {
        self.0.by_name.get(name).cloned()
    }

    /// Every installed tool with its extension, in install order.
    pub fn tools(&self) -> Vec<(Arc<Extension>, Arc<ToolRegistration>)> {
        let extensions = &self.0.extensions;
        extensions.iter().flat_map(|e| e.tools.iter().map(|tool| (e.clone(), tool.clone()))).collect()
    }

    pub fn sections(&self) -> Vec<(Arc<Extension>, Arc<PromptSection>)> {
        let extensions = &self.0.extensions;
        extensions.iter().flat_map(|e| e.sections.iter().map(|section| (e.clone(), section.clone()))).collect()
    }

    /// Built-in and installed task definitions.
    pub fn tasks(&self) -> Vec<Task> {
        self.0.tasks.clone()
    }

    pub fn task(&self, name: &str) -> Option<Task> {
        self.0.task_index.get(name).map(|index| self.0.tasks[*index].clone())
    }
}

/// Unsubscribes a registry listener; idempotent.
pub type RegistryUnsubscribe = Box<dyn FnOnce() + Send>;
pub type RegistryListener = Arc<dyn Fn() + Send + Sync>;

/// Read side of a registry consumed by a Harness.
pub trait RegistryReader: Send + Sync {
    fn snapshot(&self) -> RegistrySnapshot;
    /// Called synchronously after every publication.
    fn subscribe(&self, listener: RegistryListener) -> RegistryUnsubscribe;
}

struct RegistryInner {
    current: RegistrySnapshot,
    listeners: Vec<(u64, RegistryListener)>,
    next: u64,
}

/// Application-owned registry of extensions. Cheap to clone; clones share the registry.
#[derive(Clone)]
pub struct Registry(Arc<Mutex<RegistryInner>>);

/// Create a registry holding only the built-in tasks.
pub fn create_registry() -> Registry {
    let current =
        RegistrySnapshot::build(Vec::new()).unwrap_or_else(|_| unreachable!("built-in task names are unique"));
    Registry(Arc::new(Mutex::new(RegistryInner { current, listeners: Vec::new(), next: 0 })))
}

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Install `extension`, or replace the installed extension with its name in place. Publishes at once.
    pub fn install(&self, extension: impl Into<Arc<Extension>>) -> DurableResult<()> {
        let extension = extension.into();
        validate_extension(&extension)?;
        let current = self.lock().current.installed().to_vec();
        let next = match current.iter().position(|installed| installed.name == extension.name) {
            Some(index) => {
                let mut next = current;
                next[index] = extension;
                next
            }
            None => {
                let mut next = current;
                next.push(extension);
                next
            }
        };
        self.publish(next)
    }

    /// Remove the installed extension named `name`. A later install appends.
    pub fn uninstall(&self, name: &str) -> DurableResult<()> {
        let current = self.lock().current.installed().to_vec();
        if !current.iter().any(|installed| installed.name == name) {
            return Ok(());
        }
        self.publish(current.into_iter().filter(|installed| installed.name != name).collect())
    }

    fn publish(&self, extensions: Vec<Arc<Extension>>) -> DurableResult<()> {
        let snapshot = RegistrySnapshot::build(extensions)?;
        let listeners: Vec<RegistryListener> = {
            let mut inner = self.lock();
            inner.current = snapshot;
            inner.listeners.iter().map(|(_, listener)| listener.clone()).collect()
        };
        for listener in listeners {
            listener();
        }
        Ok(())
    }
}

impl RegistryReader for Registry {
    fn snapshot(&self) -> RegistrySnapshot {
        self.lock().current.clone()
    }

    fn subscribe(&self, listener: RegistryListener) -> RegistryUnsubscribe {
        let id = {
            let mut inner = self.lock();
            inner.next += 1;
            let id = inner.next;
            inner.listeners.push((id, listener));
            id
        };
        let weak = Arc::downgrade(&self.0);
        Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .listeners
                    .retain(|(other, _)| *other != id);
            }
        })
    }
}

fn is_section_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Unique tool names and section keys within one extension; valid, unreserved section keys.
fn validate_extension(extension: &Extension) -> DurableResult<()> {
    let mut tools = HashSet::new();
    for tool in &extension.tools {
        if !tools.insert(tool.name()) {
            return Err(DurableError::error(format!(
                "Extension {} has two tools named {}",
                extension.name,
                tool.name()
            )));
        }
    }
    let mut sections = HashSet::new();
    for section in &extension.sections {
        let key = section.key.as_str();
        if !is_section_key(key) {
            let quoted = pi_chord::Json::from(key).to_string();
            return Err(DurableError::type_error(format!("Section key {quoted} must match /^[a-z][a-z0-9_-]*$/")));
        }
        if key == INSTRUCTIONS_KEY {
            return Err(DurableError::error(format!("Section key {key} is reserved for the agent's instructions")));
        }
        if !sections.insert(key) {
            let message = format!("Extension {} has two sections with key {key}", extension.name);
            return Err(DurableError::error(message));
        }
    }
    Ok(())
}
