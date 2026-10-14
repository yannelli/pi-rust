//! Message port endpoints shared by every runtime in the process, for `MessageChannel`,
//! `BroadcastChannel`, and workers. A post queues serialized bytes on the peer endpoint and
//! wakes the thread that owns it. Also holds the environment stores behind `SHARE_ENV`.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

use rquickjs::{Array, Ctx, Function, Object, Persistent, TypedArray, Value};

use super::set_fn;
use super::shared::SharedBlock;
use crate::runtime::convert::{bytes_of, bytes_to_js};
use crate::runtime::state::{Message, state};
use crate::runtime::{UncaughtKind, report_error};

/// One message: the serialized value, transferred `ArrayBuffer` contents, port ids, and the
/// memory of each SharedArrayBuffer in it.
#[derive(Clone)]
struct Envelope {
    data: Vec<u8>,
    buffers: Vec<Vec<u8>>,
    ports: Vec<u64>,
    shared: Vec<SharedBlock>,
}

#[derive(Default)]
struct Endpoint {
    peer: Option<u64>,
    peer_closed: bool,
    queue: VecDeque<Envelope>,
    owner: Option<std::sync::mpsc::Sender<Message>>,
    /// The `BroadcastChannel` name this endpoint joined.
    group: Option<String>,
}

static PORTS: LazyLock<Mutex<HashMap<u64, Endpoint>>> = LazyLock::new(Default::default);
static ENVS: LazyLock<Mutex<HashMap<u64, BTreeMap<String, String>>>> = LazyLock::new(Default::default);
static NEXT: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// Callback id of this JavaScript thread's port dispatcher.
    static DISPATCHER: Cell<u64> = const { Cell::new(0) };
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn notify(endpoint: &Endpoint, id: u64, kind: &'static str) {
    if let Some(owner) = &endpoint.owner {
        let _ = owner.send(Message::Task(Box::new(move |ctx| dispatch(ctx, kind, id))));
    }
}

fn dispatch(ctx: &Ctx<'_>, kind: &str, id: u64) {
    let callback = state().callbacks.borrow().get(&DISPATCHER.get()).cloned();
    let Some(function) = callback.and_then(|callback| callback.restore(ctx).ok()) else { return };
    if let Err(error) = function.call::<_, ()>((kind, id)) {
        report_error(ctx, UncaughtKind::Exception, error);
    }
}

fn channel() -> (u64, u64) {
    let first = NEXT.fetch_add(2, Ordering::Relaxed);
    let mut ports = lock(&PORTS);
    ports.insert(first, Endpoint { peer: Some(first + 1), ..Default::default() });
    ports.insert(first + 1, Endpoint { peer: Some(first), ..Default::default() });
    (first, first + 1)
}

fn queue(ports: &mut HashMap<u64, Endpoint>, target: u64, envelope: Envelope) {
    if let Some(endpoint) = ports.get_mut(&target) {
        endpoint.queue.push_back(envelope);
        notify(endpoint, target, "message");
    }
}

/// Queue a message for the peer of `id`; false when the peer is gone.
fn post_envelope(id: u64, envelope: Envelope) -> bool {
    let mut ports = lock(&PORTS);
    let Some(peer) = ports.get(&id).and_then(|endpoint| endpoint.peer) else { return false };
    queue(&mut ports, peer, envelope);
    true
}

fn broadcast(id: u64, envelope: Envelope) {
    let mut ports = lock(&PORTS);
    let Some(group) = ports.get(&id).and_then(|endpoint| endpoint.group.clone()) else { return };
    let targets: Vec<u64> = ports
        .iter()
        .filter(|(other, endpoint)| **other != id && endpoint.group.as_ref() == Some(&group))
        .map(|(other, _)| *other)
        .collect();
    for target in targets {
        queue(&mut ports, target, envelope.clone());
    }
}

fn close(id: u64) {
    let mut ports = lock(&PORTS);
    let Some(endpoint) = ports.remove(&id) else { return };
    if let Some(peer) = endpoint.peer.and_then(|peer| ports.get_mut(&peer).map(|endpoint| (peer, endpoint))) {
        peer.1.peer = None;
        peer.1.peer_closed = true;
        notify(peer.1, peer.0, "close");
    }
}

/// Make the current thread the owner of `id`, or release it (`attach` false) for a transfer.
fn attach(id: u64, attach: bool) {
    let mut ports = lock(&PORTS);
    let Some(endpoint) = ports.get_mut(&id) else { return };
    endpoint.owner = attach.then(|| state().sender.clone());
    if attach && !endpoint.queue.is_empty() {
        notify(endpoint, id, "message");
    }
    if attach && endpoint.peer_closed {
        notify(endpoint, id, "close");
    }
}

fn receive<'js>(ctx: Ctx<'js>, id: u64) -> rquickjs::Result<Value<'js>> {
    let Some(envelope) = lock(&PORTS).get_mut(&id).and_then(|endpoint| endpoint.queue.pop_front()) else {
        return Ok(Value::new_undefined(ctx));
    };
    let result = Object::new(ctx.clone())?;
    result.set("data", bytes_to_js(&ctx, envelope.data)?)?;
    let buffers = Array::new(ctx.clone())?;
    for (index, bytes) in envelope.buffers.into_iter().enumerate() {
        buffers.set(index, bytes_to_js(&ctx, bytes)?)?;
    }
    result.set("buffers", buffers)?;
    result.set("ports", envelope.ports)?;
    let shared = Array::new(ctx.clone())?;
    for (index, block) in envelope.shared.iter().enumerate() {
        shared.set(index, block.adopt(&ctx)?)?;
    }
    result.set("shared", shared)?;
    Ok(result.into_value())
}

/// References to the memory of `buffers`, whose maxByteLength values (0 when not growable)
/// come in `maxima`.
fn shared_blocks<'js>(
    ctx: &Ctx<'js>,
    buffers: Vec<Value<'js>>,
    maxima: Vec<f64>,
) -> rquickjs::Result<Vec<SharedBlock>> {
    let max = |index: usize| maxima.get(index).map(|max| *max as usize);
    buffers.iter().enumerate().map(|(index, buffer)| SharedBlock::of(ctx, buffer, max(index))).collect()
}

#[allow(clippy::too_many_arguments)]
fn post<'js>(
    ctx: Ctx<'js>,
    id: u64,
    data: TypedArray<'js, u8>,
    buffers: Vec<TypedArray<'js, u8>>,
    ports: Vec<u64>,
    shared: Vec<Value<'js>>,
    maxima: Vec<f64>,
) -> rquickjs::Result<bool> {
    let shared = shared_blocks(&ctx, shared, maxima)?;
    let buffers = buffers.iter().map(bytes_of).collect();
    Ok(post_envelope(id, Envelope { data: bytes_of(&data), buffers, ports, shared }))
}

fn join(name: String) -> u64 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    lock(&PORTS).insert(id, Endpoint { group: Some(name), ..Default::default() });
    id
}

fn env_natives(object: &Object<'_>) -> rquickjs::Result<()> {
    set_fn(object, "envCreate", |entries: HashMap<String, String>| {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        lock(&ENVS).insert(id, entries.into_iter().collect());
        id
    })?;
    set_fn(object, "envGet", |id: u64, key: String| lock(&ENVS).get(&id).and_then(|env| env.get(&key).cloned()))?;
    set_fn(object, "envSet", |id: u64, key: String, value: String| {
        lock(&ENVS).entry(id).or_default().insert(key, value);
    })?;
    set_fn(object, "envDelete", |id: u64, key: String| {
        lock(&ENVS).entry(id).or_default().remove(&key);
    })?;
    set_fn(object, "envKeys", |id: u64| lock(&ENVS).get(&id).map(|env| env.keys().cloned().collect::<Vec<_>>()))?;
    Ok(())
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "setDispatcher", |ctx: Ctx<'js>, callback: Function<'js>| {
        let state = state();
        let id = state.next_id();
        state.callbacks.borrow_mut().insert(id, Persistent::save(&ctx, callback));
        DISPATCHER.set(id);
    })?;
    set_fn(&object, "channel", || {
        let (first, second) = channel();
        vec![first, second]
    })?;
    set_fn(&object, "attach", |id: u64| attach(id, true))?;
    set_fn(&object, "detach", |id: u64| attach(id, false))?;
    set_fn(&object, "post", post)?;
    set_fn(&object, "receive", receive)?;
    set_fn(&object, "close", close)?;
    set_fn(&object, "join", join)?;
    set_fn(
        &object,
        "broadcast",
        |ctx: Ctx<'js>, id: u64, data: TypedArray<'js, u8>, shared: Vec<Value<'js>>, maxima: Vec<f64>| {
            let shared = shared_blocks(&ctx, shared, maxima)?;
            broadcast(id, Envelope { data: bytes_of(&data), buffers: Vec::new(), ports: Vec::new(), shared });
            Ok::<_, rquickjs::Error>(())
        },
    )?;
    // A new SharedArrayBuffer over the same memory, for structuredClone in this thread.
    set_fn(&object, "cloneShared", |ctx: Ctx<'js>, buffer: Value<'js>, max: f64| {
        SharedBlock::of(&ctx, &buffer, Some(max as usize))?.adopt(&ctx)
    })?;
    set_fn(&object, "busy", |busy: bool| if busy { state().add_busy() } else { state().remove_busy() })?;
    env_natives(&object)?;
    super::worker::natives(&object)?;
    Ok(object)
}
