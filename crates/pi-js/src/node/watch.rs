//! `fs.watch` on the platform's file events (inotify, FSEvents, ReadDirectoryChangesW) via
//! notify, reporting Node's `rename` and `change` event types.

use std::path::Path;

use notify::event::{EventKind, ModifyKind};
use notify::{RecursiveMode, Watcher};
use rquickjs::{Ctx, Function, Object, Persistent};

use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{error_value, throw};
use crate::runtime::state::{Message, Task, state};
use crate::runtime::{UncaughtKind, report_error};

struct FsWatcher {
    _watcher: notify::RecommendedWatcher,
    referenced: bool,
}

enum Event {
    Change(&'static str, String),
    Error(JsError),
}

fn deliver(ctx: &Ctx<'_>, id: u64, event: Event) {
    let state = state();
    if state.with_resource(id, |_: &mut FsWatcher| ()).is_err() {
        return;
    }
    let callback = state.callbacks.borrow().get(&id).cloned();
    let Some(function) = callback.and_then(|callback| callback.restore(ctx).ok()) else { return };
    let result = match event {
        Event::Change(kind, name) => function.call::<_, ()>((kind, name)),
        Event::Error(error) => error_value(ctx, &error).and_then(|error| function.call::<_, ()>(("error", error))),
    };
    if let Err(error) = result {
        report_error(ctx, UncaughtKind::Exception, error);
    }
}

/// The name Node reports: relative to a watched directory, or the watched file's own name.
fn relative_name(target: &Path, is_dir: bool, path: &Path) -> String {
    let name = if is_dir {
        path.strip_prefix(target).unwrap_or(path)
    } else {
        Path::new(path.file_name().unwrap_or_default())
    };
    name.display().to_string()
}

fn event_type(kind: &EventKind) -> Option<&'static str> {
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => Some("rename"),
        EventKind::Modify(_) | EventKind::Any => Some("change"),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn file_identity(metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(target_os = "macos"))]
fn file_identity(_metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

fn same_file(path: &Path, identity: Option<(u64, u64)>) -> bool {
    identity.is_some() && std::fs::metadata(path).ok().as_ref().and_then(file_identity) == identity
}

fn watch<'js>(ctx: Ctx<'js>, path: String, recursive: bool, callback: Function<'js>) -> rquickjs::Result<u64> {
    let state = state();
    let target = crate::resolve::canonical(&state.cwd.borrow().join(&path));
    let metadata = std::fs::metadata(&target).map_err(|error| {
        throw(&ctx, &JsError::from_io(&error, "watch", Some(&path)).with_data("filename", path.clone()))
    })?;
    let is_dir = metadata.is_dir();
    let identity = file_identity(&metadata);
    let id = state.next_id();
    let sender = state.sender.clone();
    let root = target.clone();
    let post = move |event: Event| {
        let task: Task = Box::new(move |ctx| deliver(ctx, id, event));
        let _ = sender.send(Message::Task(task));
    };
    let handler = move |result: notify::Result<notify::Event>| match result {
        Ok(event) => {
            let Some(mut kind) = event_type(&event.kind) else { return };
            // libuv watches a single file with kqueue on macOS, so a write is `change`. FSEvents can repeat a
            // stale "created" flag on a write; the same inode means the file was not replaced.
            if !is_dir && matches!(event.kind, EventKind::Create(_)) && same_file(&root, identity) {
                kind = "change";
            }
            for changed in &event.paths {
                if !is_dir && changed != &root {
                    continue;
                }
                post(Event::Change(kind, relative_name(&root, is_dir, changed)));
            }
        }
        Err(error) => {
            post(Event::Error(JsError::new(error.to_string()).with_code("EIO").with_data("syscall", "watch")))
        }
    };
    let mut watcher = notify::recommended_watcher(handler).map_err(|error| {
        throw(&ctx, &JsError::new(error.to_string()).with_code("EMFILE").with_data("syscall", "watch"))
    })?;
    let mode = if is_dir && recursive { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
    watcher.watch(&target, mode).map_err(|error| {
        throw(&ctx, &JsError::new(error.to_string()).with_code("EIO").with_data("syscall", "watch"))
    })?;
    state.resources.borrow_mut().insert(id, Box::new(FsWatcher { _watcher: watcher, referenced: false }));
    state.callbacks.borrow_mut().insert(id, Persistent::save(&ctx, callback));
    Ok(id)
}

fn set_ref(id: u64, referenced: bool) {
    let state = state();
    let previous =
        state.with_resource(id, |watcher: &mut FsWatcher| std::mem::replace(&mut watcher.referenced, referenced));
    match previous {
        Ok(previous) if previous != referenced && referenced => state.add_busy(),
        Ok(previous) if previous != referenced => state.remove_busy(),
        _ => {}
    }
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "watch", watch)?;
    set_fn(&object, "ref", set_ref)?;
    set_fn(&object, "close", |id: u64| {
        set_ref(id, false);
        let state = state();
        state.resources.borrow_mut().remove(&id);
        state.callbacks.borrow_mut().remove(&id);
    })?;
    Ok(object)
}
