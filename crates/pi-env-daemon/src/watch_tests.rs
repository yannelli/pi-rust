//! The native watch bookkeeping: a watch the backend dropped is installed again (upstream issue 11342).

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use notify::event::RemoveKind;

use super::*;

/// A new empty directory under the system temp directory.
fn temp_dir() -> PathBuf {
    let nanos = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("pi-env-watch-rearm-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A native watcher of `dir`, recursive, and the native events it gets.
fn watcher<'a>(
    dir: &str,
    output: &'a Output,
    control: &'a Control,
) -> (FileWatcher<'a>, Receiver<notify::Result<Event>>) {
    let targets = parse_targets(&json!({ "targets": [{ "path": dir, "recursive": true }] })).unwrap();
    let (events_sender, events) = mpsc::channel();
    let watcher = FileWatcher {
        id: 1,
        output,
        control,
        targets,
        mode: Mode::Native,
        poll_interval: Duration::from_millis(DEFAULT_POLL_MS),
        max_directories: DEFAULT_MAX_DIRECTORIES,
        snapshot: Snapshot::new(),
        native: None,
        events_sender,
        installed: HashMap::new(),
        events: HashSet::new(),
        settle_at: None,
    };
    (watcher, events)
}

/// Whether a native event about `path` arrives within two seconds.
fn event_about(events: &Receiver<notify::Result<Event>>, path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while let Ok(event) = events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if event.is_ok_and(|event| event.paths.iter().any(|reported| reported == path)) {
            return true;
        }
    }
    false
}

#[test]
fn watches_a_directory_again_once_the_backend_dropped_its_watch_while_it_kept_its_inode() {
    let root = temp_dir();
    let child = root.join("a");
    fs::create_dir(&child).unwrap();
    let (root_path, child_path) = (root.to_string_lossy().into_owned(), child.to_string_lossy().into_owned());
    let (output, control) = (Output::default(), Control::default());
    let (mut watcher, events) = watcher(&root_path, &output, &control);
    assert!(watcher.sync(false).is_ok());
    assert!(watcher.installed.contains_key(&child_path));
    // The kernel drops the watch of `a`, which keeps its inode (IN_DELETE_SELF, then IN_IGNORED); notify drops its
    // watch and reports the removal of `a`.
    watcher.native.as_mut().unwrap().unwatch(&child).unwrap();
    watcher.on_event(Ok(Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(child.clone())));
    assert!(watcher.sync(true).is_ok());
    assert!(watcher.installed.contains_key(&child_path));
    while events.try_recv().is_ok() {}
    fs::write(child.join("y.md"), "x").unwrap();
    assert!(event_about(&events, &child.join("y.md")), "no event from the watch of a");
    watcher.native = None;
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn keeps_the_watch_of_a_directory_on_other_events() {
    let root = temp_dir();
    let child = root.join("a");
    fs::create_dir(&child).unwrap();
    let (output, control) = (Output::default(), Control::default());
    let (mut watcher, _events) = watcher(&root.to_string_lossy(), &output, &control);
    assert!(watcher.sync(false).is_ok());
    let modified = EventKind::Modify(notify::event::ModifyKind::Any);
    watcher.on_event(Ok(Event::new(modified).add_path(child.clone())));
    assert!(watcher.installed.contains_key(&*child.to_string_lossy()));
    watcher.native = None;
    fs::remove_dir_all(&root).unwrap();
}
