//! The native watch bookkeeping: a watch the backend dropped is installed again (upstream issue 11342).

use super::*;

/// A new empty directory under the system temp directory.
fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("pi-watch-rearm-").tempdir().unwrap()
}

/// A native watcher core of `dir`, recursive, and the native events it gets.
fn core(dir: &str) -> (Core, mpsc::UnboundedReceiver<Message>) {
    let target = ResolvedTarget::new(&WatchTarget { path: dir.to_owned(), recursive: true, exclude: None });
    let on_change: WatchListener = Arc::new(|_| {});
    let shared = Arc::new(Shared { closed: Mutex::new(false), on_change, mode: Mutex::new(WatchMode::Native) });
    let (messages, receiver) = mpsc::unbounded_channel();
    let core = Core {
        targets: Arc::new(vec![target]),
        shared,
        messages,
        poll_interval: Duration::from_millis(DEFAULT_POLL_MS),
        max_directories: DEFAULT_MAX_DIRECTORIES,
        snapshot: Snapshot::new(),
        watcher: None,
        installed: HashMap::new(),
        aliases: Vec::new(),
        events: BTreeSet::new(),
        flush_at: None,
        settle_at: None,
        poll_at: None,
        stopped: false,
    };
    (core, receiver)
}

/// Whether a native event about `path` arrives within two seconds.
async fn event_about(receiver: &mut mpsc::UnboundedReceiver<Message>, path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while let Ok(Some(message)) = tokio::time::timeout_at(deadline, receiver.recv()).await {
        if let Message::Event { paths, .. } = message
            && paths.iter().any(|reported| reported == path)
        {
            return true;
        }
    }
    false
}

#[tokio::test]
async fn watches_a_directory_again_once_the_backend_dropped_its_watch_while_it_kept_its_inode() {
    let directory = temp_dir();
    let root = directory.path();
    let child = root.join("a");
    std::fs::create_dir(&child).unwrap();
    let (root_path, child_path) = (root.to_string_lossy().into_owned(), child.to_string_lossy().into_owned());
    let (mut core, mut receiver) = core(&root_path);
    core.sync(false).await.unwrap();
    assert!(core.installed.contains_key(&child_path));
    // The kernel drops the watch of `a`, which keeps its inode (IN_DELETE_SELF, then IN_IGNORED); notify drops its
    // watch and reports the removal of `a`.
    core.watcher.as_mut().unwrap().unwatch(&child).unwrap();
    core.on_event(vec![child.clone()], true);
    core.sync(true).await.unwrap();
    assert!(core.installed.contains_key(&child_path));
    while receiver.try_recv().is_ok() {}
    std::fs::write(child.join("y.md"), "x").unwrap();
    assert!(event_about(&mut receiver, &child.join("y.md")).await, "no event from the watch of a");
    core.watcher = None;
    directory.close().unwrap();
}

#[tokio::test]
async fn keeps_the_watch_of_a_directory_on_other_events() {
    let directory = temp_dir();
    let root = directory.path();
    let child = root.join("a");
    std::fs::create_dir(&child).unwrap();
    let child_path = child.to_string_lossy().into_owned();
    let (mut core, _receiver) = core(&root.to_string_lossy());
    core.sync(false).await.unwrap();
    core.on_event(vec![child.clone()], false);
    assert!(core.installed.contains_key(&child_path));
    core.watcher = None;
    directory.close().unwrap();
}
