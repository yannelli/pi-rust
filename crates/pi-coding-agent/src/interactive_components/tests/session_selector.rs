//! The `/resume` session selector; ports `session-selector-path-delete.test.ts` and
//! `session-selector-rename.test.ts`. Search tests live in the selector's `search` module.

mod flows;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, UNIX_EPOCH};

use pi_tui::Component;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::interactive_components::session_selector::{
    RenameSessionFn, SessionListProgress, SessionSelector, SessionSelectorOptions, SessionsFuture,
};
use crate::interactive_components::test_support::{Calls, keys, local, render_text, setup};
use crate::session::{SessionInfo, parse_iso};

const CTRL_D: &str = "\x04";
/// Kitty encoding of Ctrl+R.
const CTRL_R: &str = "\x1b[114;5u";

pub(super) fn make_session(id: &str) -> SessionInfo {
    SessionInfo {
        path: PathBuf::from(format!("/tmp/{id}.jsonl")),
        id: id.to_string(),
        cwd: String::new(),
        name: None,
        parent_session_path: None,
        created: 0,
        modified: 0,
        changed: UNIX_EPOCH,
        message_count: 1,
        first_message: "hello".into(),
        all_messages_text: "hello".into(),
    }
}

pub(super) fn named(id: &str, name: &str, modified: &str) -> SessionInfo {
    let modified = parse_iso(modified).expect("timestamp");
    SessionInfo {
        name: Some(name.to_string()),
        modified,
        changed: UNIX_EPOCH + Duration::from_millis(modified as u64),
        ..make_session(id)
    }
}

pub(super) type Loader = Box<dyn FnMut(SessionListProgress, CancellationToken) -> SessionsFuture>;

/// A loader that resolves right away.
pub(super) fn ready(sessions: Vec<SessionInfo>) -> Loader {
    Box::new(move |_, _| {
        let sessions = sessions.clone();
        Box::pin(async move { Ok(sessions) })
    })
}

/// A loader that counts calls, reports `progress` synchronously, and resolves when the sender
/// fires.
pub(super) fn deferred(progress: Option<Vec<SessionInfo>>) -> (oneshot::Sender<Vec<SessionInfo>>, Loader, Calls<()>) {
    let (sender, receiver) = oneshot::channel();
    let receiver = RefCell::new(Some(receiver));
    let calls = Calls::new();
    let counter = calls.clone();
    let loader: Loader = Box::new(move |report: SessionListProgress, _| {
        counter.push(());
        if let Some(partial) = &progress {
            report(1, 2, Some(partial));
        }
        let receiver = receiver.borrow_mut().take().expect("a single load");
        Box::pin(async move { Ok(receiver.await?) })
    });
    (sender, loader, calls)
}

pub(super) fn build(
    current: Loader,
    all: Loader,
    options: SessionSelectorOptions,
    current_path: Option<&Path>,
) -> Rc<RefCell<SessionSelector>> {
    SessionSelector::new(current, all, |_| {}, || {}, || {}, || {}, options, current_path)
}

pub(super) fn text(selector: &Rc<RefCell<SessionSelector>>, width: usize) -> String {
    render_text(&mut *selector.borrow_mut(), width)
}

pub(super) fn send(selector: &Rc<RefCell<SessionSelector>>, data: &str) {
    selector.borrow_mut().handle_input(data);
}

/// Lets `spawn_local` tasks run.
pub(super) async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

fn confirmation_recorder(selector: &Rc<RefCell<SessionSelector>>) -> Calls<Option<PathBuf>> {
    let changes = Calls::new();
    selector.borrow().session_list().borrow_mut().on_delete_confirmation_change = Some(Box::new(changes.recorder()));
    changes
}

fn two_sessions() -> Vec<SessionInfo> {
    vec![make_session("a"), make_session("b")]
}

#[test]
fn ctrl_backspace_is_not_delete_when_the_query_is_not_empty() {
    let _kb = setup();
    let selector = build(ready(two_sessions()), ready(Vec::new()), SessionSelectorOptions::default(), None);
    let changes = confirmation_recorder(&selector);
    send(&selector, "a");
    send(&selector, keys::CTRL_BACKSPACE);
    assert!(changes.is_empty());
}

#[test]
fn ctrl_d_confirms_even_with_a_query() {
    let _kb = setup();
    let sessions = two_sessions();
    let selector = build(ready(sessions.clone()), ready(Vec::new()), SessionSelectorOptions::default(), None);
    let changes = confirmation_recorder(&selector);
    send(&selector, "a");
    send(&selector, CTRL_D);
    assert_eq!(changes.get(), [Some(sessions[0].path.clone())]);
}

#[test]
fn ctrl_backspace_confirms_with_an_empty_query_and_enter_deletes() {
    let _kb = setup();
    let sessions = two_sessions();
    let selector = build(ready(sessions.clone()), ready(Vec::new()), SessionSelectorOptions::default(), None);
    let changes = confirmation_recorder(&selector);
    let deleted = Calls::new();
    selector.borrow().session_list().borrow_mut().on_delete_session = Some(Box::new(deleted.recorder()));
    send(&selector, keys::CTRL_BACKSPACE);
    assert_eq!(changes.get(), [Some(sessions[0].path.clone())]);
    send(&selector, "\r");
    assert_eq!(changes.get(), [Some(sessions[0].path.clone()), None]);
    assert_eq!(deleted.get(), [sessions[0].path.clone()]);
}

#[tokio::test(flavor = "current_thread")]
async fn late_all_load_does_not_switch_scope_back() {
    local(async {
        let _kb = setup();
        let (sender, all, calls) = deferred(None);
        let current = ready(vec![make_session("current")]);
        let selector = build(current, all, SessionSelectorOptions::default(), None);
        settle().await;
        send(&selector, keys::TAB);
        send(&selector, keys::TAB);
        sender.send(vec![make_session("all")]).unwrap();
        settle().await;
        assert_eq!(calls.len(), 1);
        let output = text(&selector, 120);
        assert!(output.contains("Resume Session (Current Folder)"), "{output}");
        assert!(!output.contains("Resume Session (All)"));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn toggling_scopes_during_an_all_load_does_not_start_another() {
    local(async {
        let _kb = setup();
        let all_sessions = vec![make_session("all")];
        let (sender, all, calls) = deferred(Some(all_sessions.clone()));
        let selector = build(ready(vec![make_session("current")]), all, SessionSelectorOptions::default(), None);
        settle().await;
        send(&selector, keys::TAB);
        send(&selector, keys::TAB);
        send(&selector, keys::TAB);
        assert_eq!(calls.len(), 1);
        let selected = selector.borrow().session_list().borrow().selected_session_path();
        assert_eq!(selected, Some(all_sessions[0].path.clone()));
        assert!(text(&selector, 120).contains("Loading"));
        sender.send(all_sessions).unwrap();
        settle().await;
        assert!(!text(&selector, 120).contains("Loading"));
    })
    .await;
}

/// `<base>/alias-a/sessions` and `<base>/alias-b/sessions` both link to one real directory.
#[cfg(unix)]
struct SymlinkedSessions {
    _base: tempfile::TempDir,
    parent_alias_a: PathBuf,
    parent_alias_b: PathBuf,
    child_alias_b: PathBuf,
}

#[cfg(unix)]
fn symlinked_session_paths() -> SymlinkedSessions {
    let base = tempfile::tempdir().unwrap();
    let shared = base.path().join("real").join("sessions");
    std::fs::create_dir_all(&shared).unwrap();
    for alias in ["alias-a", "alias-b"] {
        std::fs::create_dir_all(base.path().join(alias)).unwrap();
        std::os::unix::fs::symlink(&shared, base.path().join(alias).join("sessions")).unwrap();
    }
    std::fs::write(shared.join("parent.jsonl"), "parent\n").unwrap();
    std::fs::write(shared.join("child.jsonl"), "child\n").unwrap();
    let alias = |name: &str, file: &str| base.path().join(name).join("sessions").join(file);
    SymlinkedSessions {
        parent_alias_a: alias("alias-a", "parent.jsonl"),
        parent_alias_b: alias("alias-b", "parent.jsonl"),
        child_alias_b: alias("alias-b", "child.jsonl"),
        _base: base,
    }
}

#[cfg(unix)]
#[test]
fn threads_sessions_across_symlink_aliases() {
    let _kb = setup();
    let paths = symlinked_session_paths();
    let parent =
        SessionInfo { path: paths.parent_alias_b.clone(), ..named("parent", "Parent", "2026-01-01T00:00:00Z") };
    let child = SessionInfo {
        path: paths.child_alias_b.clone(),
        parent_session_path: Some(paths.parent_alias_a.display().to_string()),
        ..named("child", "Child", "2025-12-31T00:00:00Z")
    };
    let selector = build(ready(vec![parent, child]), ready(Vec::new()), SessionSelectorOptions::default(), None);
    let output = text(&selector, 120);
    assert!(output.contains("Parent"), "{output}");
    assert!(output.contains("└─ Child"), "{output}");
}

#[test]
fn threaded_sessions_sort_by_latest_activity_in_the_subtree() {
    let _kb = setup();
    let parent_one = named("parent-one", "Parent one", "2026-01-02T00:00:00Z");
    let parent_two = named("parent-two", "Parent two", "2026-01-01T00:00:00Z");
    let child_two = SessionInfo {
        parent_session_path: Some(parent_two.path.display().to_string()),
        ..named("child-two", "Child two", "2026-01-03T00:00:00Z")
    };
    let sessions = vec![parent_one, parent_two, child_two];
    let selector = build(ready(sessions), ready(Vec::new()), SessionSelectorOptions::default(), None);
    let output = text(&selector, 120);
    let position = |needle: &str| output.find(needle).unwrap_or_else(|| panic!("{needle} missing in {output}"));
    assert!(position("Parent two") < position("└─ Child two"));
    assert!(position("└─ Child two") < position("Parent one"));
}

#[cfg(unix)]
#[test]
fn current_session_is_active_across_symlink_aliases() {
    let _kb = setup();
    let paths = symlinked_session_paths();
    let parent =
        SessionInfo { path: paths.parent_alias_b.clone(), ..named("parent", "Parent", "2026-01-01T00:00:00Z") };
    let options = SessionSelectorOptions::default();
    let selector = build(ready(vec![parent]), ready(Vec::new()), options, Some(&paths.parent_alias_a));
    let changes = confirmation_recorder(&selector);
    let errors = Calls::new();
    selector.borrow().session_list().borrow_mut().on_error = Some(Box::new(errors.recorder()));
    send(&selector, CTRL_D);
    assert!(changes.is_empty());
    assert_eq!(errors.get(), ["Cannot delete the currently active session"]);
}

fn rename_options(show_rename_hint: bool, renames: Option<&Calls<(PathBuf, String)>>) -> SessionSelectorOptions {
    SessionSelectorOptions {
        rename_session: renames.map(|calls| {
            let calls = calls.clone();
            Box::new(move |path: PathBuf, name: String| {
                calls.push((path, name));
                Ok(())
            }) as RenameSessionFn
        }),
        show_rename_hint: Some(show_rename_hint),
    }
}

#[test]
fn shows_the_rename_hint_in_the_resume_command_picker() {
    let _kb = setup();
    let selector = build(ready(vec![make_session("a")]), ready(Vec::new()), rename_options(true, None), None);
    let output = text(&selector, 120);
    assert!(output.contains("ctrl+r") && output.contains("rename"), "{output}");
}

#[test]
fn hides_the_rename_hint_in_the_resume_flag_picker() {
    let _kb = setup();
    let selector = build(ready(vec![make_session("a")]), ready(Vec::new()), rename_options(false, None), None);
    let output = text(&selector, 120);
    assert!(!output.contains("ctrl+r") && !output.contains("rename"), "{output}");
}

#[test]
fn ctrl_r_enters_rename_mode_and_enter_submits() {
    let _kb = setup();
    let session = SessionInfo { name: Some("Old".into()), ..make_session("a") };
    let renames = Calls::new();
    let options = rename_options(true, Some(&renames));
    let selector = build(ready(vec![session.clone()]), ready(Vec::new()), options, None);
    send(&selector, CTRL_R);
    let output = text(&selector, 120);
    assert!(output.contains("Rename Session"), "{output}");
    assert!(!output.contains("Resume Session"));
    send(&selector, "X");
    send(&selector, "\r");
    assert_eq!(renames.get(), [(session.path, "XOld".to_string())]);
    assert!(text(&selector, 120).contains("Resume Session (Current Folder)"));
}
