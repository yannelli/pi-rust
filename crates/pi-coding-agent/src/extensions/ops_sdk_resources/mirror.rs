//! Calls between a child host and the host that owns its loader: mirror descriptors, handler, tool,
//! and command calls of mirrored factories, tool hooks, and `pi.events`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pi_js::{HostValue, JsError};
use serde_json::{Value, json};

use super::super::ops::{Args, error};
use super::super::runner::ExtensionRunner;
use super::ParentLink;
use crate::agent_session::lock;

const HOOK_TIMEOUT: Duration = Duration::from_secs(2);
/// The target factories see before a session binds; no session has it, so reads fail as unbound.
const UNBOUND: u64 = (1 << 53) - 1;

fn link(child: &ExtensionRunner) -> Result<(&ParentLink, Arc<ExtensionRunner>), JsError> {
    let link = child.parent.get().ok_or_else(|| error("This extension host has no SDK resource loader"))?;
    let parent = link.parent.upgrade().ok_or_else(|| error("The extension host has shut down"))?;
    Ok((link, parent))
}

/// The parent's target of the session `child` is bound to; the target exists once the session does.
fn target(child: &ExtensionRunner, link: &ParentLink, parent: &ExtensionRunner) -> u64 {
    let target = link.target.load(Ordering::SeqCst);
    match child.session() {
        Some(session) if target != 0 => {
            lock(&parent.targets).entry(target).or_insert_with(|| Arc::downgrade(&session));
            target
        }
        _ => UNBOUND,
    }
}

fn find_child(parent: &ExtensionRunner, group: u64) -> Option<Arc<ExtensionRunner>> {
    let loaded = lock(&parent.resources).loaders.values().find_map(|entry| entry.loaded.child(group));
    loaded.or_else(|| {
        let children = lock(&parent.sdk.children);
        let mut hosted = children.values().filter_map(|child| child.extensions.as_ref());
        hosted.find(|hosted| hosted.group() == group).map(|hosted| hosted.0.clone())
    })
}

/// `resources.mirror`: the descriptors of a factory group, sent on to its child host.
pub(super) fn store(parent: &ExtensionRunner, args: &Args) -> Result<Value, JsError> {
    let group = args.u64(1);
    let descriptors = args.json(2).as_array().cloned().unwrap_or_default();
    let mirrored = {
        let mut state = lock(&parent.resources);
        let loaded = &mut state.entry(args.u64(0))?.loaded;
        if loaded.group == group {
            loaded.factories = descriptors.clone();
            loaded.mirrored
        } else {
            descriptors.len()
        }
    };
    if let Some(child) = find_child(parent, group)
        && let Some(link) = child.parent.get()
    {
        for (index, descriptor) in descriptors.into_iter().enumerate().take(mirrored) {
            let _ = link.to_child.send(vec![json!("mirror"), json!(index), descriptor]);
        }
    }
    Ok(Value::Null)
}

/// `resources.deliver`: an event of the loader's bus for the child host of a group.
pub(super) fn deliver(parent: &ExtensionRunner, args: &Args) -> Value {
    if let Some(child) = find_child(parent, args.u64(0))
        && let Some(link) = child.parent.get()
    {
        let _ = link.to_child.send(vec![json!("deliver"), args.json(1), args.json(2)]);
    }
    Value::Null
}

/// `inline.*` of a child host: the descriptor of a mirror, a tool hook, or a `pi.events` call.
pub(crate) fn child_sync(child: &ExtensionRunner, name: &str, args: &Args) -> Result<HostValue, JsError> {
    let (link, parent) = link(child)?;
    let value = match name {
        "descriptor" => {
            let state = lock(&parent.resources);
            let loaded = state.loaders.get(&link.loader).map(|entry| &entry.loaded);
            let loaded = loaded.filter(|loaded| loaded.group == link.group);
            loaded.and_then(|loaded| loaded.factories.get(args.u64(0) as usize).cloned()).unwrap_or(Value::Null)
        }
        "hook" => {
            let target = target(child, link, &parent);
            let args = [json!("hook"), json!(link.group), json!(target), args.json(0)];
            let args = args.into_iter().map(HostValue::Json).collect();
            parent.host().blocking_call_method_within("sdk", args, HOOK_TIMEOUT)?.into_json()
        }
        "bus" => {
            let _ =
                link.to_parent.send(vec![json!("bus"), json!(link.group), args.json(0), args.json(1), args.json(2)]);
            Value::Null
        }
        other => return Err(error(format!("Unknown host operation: inline.{other}"))),
    };
    Ok(HostValue::Json(value))
}

/// `inline.forward`: a handler, tool, command, or completion call of a mirror, run by the factory
/// in the parent host. The call's signal, update callback, and nested tool context go with it.
pub(crate) async fn forward(child: &Arc<ExtensionRunner>, args: &Args) -> Result<Value, JsError> {
    let (link, parent) = link(child)?;
    let target = target(child, link, &parent);
    let mut payload = args.json(1);
    let signal = payload["signal"].as_u64().map(|id| parent.signal_for(&child.token(Some(id))));
    payload["signal"] = json!(signal);
    let update = payload["updateKey"].as_u64().and_then(|key| lock(&child.updates).get(&key).cloned());
    let update = update.map(|callback| {
        let key = parent.next_id();
        lock(&parent.updates).insert(key, callback);
        key
    });
    payload["updateKey"] = json!(update);
    let call_id = payload["toolCallId"].as_str().map(str::to_string);
    let context = call_id.as_ref().and_then(|id| lock(&child.nested).get(id).cloned());
    if let (Some(id), Some(context)) = (&call_id, context) {
        lock(&parent.nested).insert(id.clone(), context);
    }
    let call = vec![json!("invoke"), json!(link.group), args.json(0), json!(target), payload];
    let result = parent.call_json("sdk", call).await;
    if let Some(id) = signal {
        parent.release_signal(id);
    }
    if let Some(key) = update {
        lock(&parent.updates).remove(&key);
    }
    if let Some(id) = call_id {
        lock(&parent.nested).remove(&id);
    }
    result
}

/// `resources.models`: a provider or virtual model call of factory group `args[0]`. It binds to the
/// runtime of the group's child host, and its callbacks run in this host's maps of that group.
pub(super) fn group_models(parent: &ExtensionRunner, args: &Args) -> Result<Value, JsError> {
    let group = args.u64(0);
    let child = lock(&parent.resources).hosts.get(&group).and_then(std::sync::Weak::upgrade);
    let child = child.ok_or_else(|| error("The session of these extension factories has ended"))?;
    let callbacks = parent.this();
    match args.str(1)?.as_str() {
        "provider.register" => {
            child.register_provider_for(&callbacks, group, &args.str(2)?, args.json(3), &args.str(4)?).map_err(error)?
        }
        "provider.unregister" => child.unregister_provider(&args.str(2)?),
        "virtualModel.register" => child.register_virtual_model_for(&callbacks, group, args.json(2)).map_err(error)?,
        "virtualModel.unregister" => child.unregister_virtual_model(&args.str(2)?, &args.str(3)?),
        other => return Err(error(format!("Unknown model operation: {other}"))),
    }
    Ok(Value::Null)
}
