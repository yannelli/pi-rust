//! VirtualTerminal flows of the session selector: rendering, navigation, toggles, search, and
//! delete and rename against real session files.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use pi_tui::Tui;
use serde_json::json;

use super::{Loader, deferred, make_session, named, ready, settle};
use crate::interactive_components::session_selector::{
    RenameSessionFn, SessionSelector, SessionSelectorOptions, all_sessions_loader, format_session_age,
    project_sessions_loader,
};
use crate::interactive_components::test_support::{Calls, Harness, keys, local};
use crate::session::listing::rename_session;
use crate::session::{SessionInfo, session_dir};

type Selector = Harness<SessionSelector>;

fn mount(tui: &Tui, current: Loader, all: Loader, options: SessionSelectorOptions) -> Rc<RefCell<SessionSelector>> {
    let weak = tui.downgrade();
    let render = move || {
        if let Some(tui) = weak.upgrade() {
            tui.request_render(false);
        }
    };
    SessionSelector::new(current, all, |_| {}, || {}, || {}, render, options, None)
}

fn harness(sessions: Vec<SessionInfo>) -> Selector {
    Harness::new_shared(120, 40, |tui| mount(tui, ready(sessions), ready(Vec::new()), Default::default()))
}

fn session_with_message(id: &str, message: &str) -> SessionInfo {
    SessionInfo { first_message: message.into(), all_messages_text: message.into(), ..make_session(id) }
}

fn selected_row(harness: &Selector) -> String {
    harness.viewport().into_iter().find(|line| line.starts_with("› ")).unwrap_or_default()
}

#[test]
fn renders_header_hints_and_rows() {
    let harness = harness(vec![session_with_message("a", "first\tmessage"), make_session("b")]);
    let screen = harness.screen();
    let header = harness.viewport().into_iter().find(|line| line.contains("Resume Session")).unwrap();
    assert!(header.starts_with("Resume Session (Current Folder)"), "{header}");
    assert!(header.ends_with("◉ Current Folder | ○ All  Name: All  Sort: Threaded"), "{header}");
    assert!(screen.contains("tab scope · re:<pattern> regex · \"phrase\" exact"), "{screen}");
    assert!(screen.contains("ctrl+s sort · ctrl+n named · ctrl+d delete · ctrl+p path (off)"), "{screen}");
    assert!(!screen.contains("rename"));
    assert!(selected_row(&harness).starts_with("› first message"), "{screen}");
    assert!(harness.viewport()[1].starts_with("────"));
}

#[test]
fn session_age_uses_upstream_buckets() {
    let minute = 60_000;
    let day = 1440 * minute;
    let cases = [(0, "now"), (5 * minute, "5m"), (180 * minute, "3h"), (2 * day, "2d"), (14 * day, "2w")];
    for (age, expected) in cases.into_iter().chain([(120 * day, "4mo"), (800 * day, "2y"), (-minute, "now")]) {
        assert_eq!(format_session_age(1_000_000_000_000 - age, 1_000_000_000_000), expected);
    }
}

#[test]
fn navigation_selects_and_cancels() {
    let selected = Calls::new();
    let cancelled = Calls::new();
    let sessions: Vec<SessionInfo> = ["a", "b", "c"].map(|id| session_with_message(id, &format!("msg {id}"))).into();
    let (on_select, on_cancel) = (selected.recorder(), cancelled.counter());
    let harness = Harness::new_shared(120, 40, move |_| {
        let options = SessionSelectorOptions::default();
        SessionSelector::new(ready(sessions), ready(Vec::new()), on_select, on_cancel, || {}, || {}, options, None)
    });
    harness.send_all(&[keys::DOWN, keys::DOWN, keys::DOWN]);
    assert!(selected_row(&harness).starts_with("› msg c"));
    harness.send(keys::UP);
    harness.send(keys::ENTER);
    assert_eq!(selected.get(), [PathBuf::from("/tmp/b.jsonl")]);
    harness.send(keys::ESCAPE);
    assert_eq!(cancelled.len(), 1);
}

#[test]
fn long_lists_page_and_show_position() {
    let sessions: Vec<SessionInfo> =
        (0..25).map(|n| session_with_message(&format!("s{n}"), &format!("msg {n}"))).collect();
    let harness = harness(sessions);
    assert!(harness.screen().contains("  (1/25)"));
    harness.send(keys::PAGE_DOWN);
    assert!(selected_row(&harness).starts_with("› msg 10"));
    assert!(harness.screen().contains("  (11/25)"));
    harness.send(keys::PAGE_UP);
    assert!(selected_row(&harness).starts_with("› msg 0"));
}

#[test]
fn sort_and_name_filter_toggles() {
    let sessions = vec![make_session("a"), named("n", "Named one", "2026-01-01T00:00:00Z")];
    let harness = harness(sessions);
    for label in ["Sort: Recent", "Sort: Fuzzy", "Sort: Threaded"] {
        harness.send(&keys::ctrl('s'));
        assert!(harness.screen().contains(label), "{label}");
    }
    harness.send(&keys::ctrl('n'));
    let screen = harness.screen();
    assert!(screen.contains("Name: Named") && screen.contains("Named one"), "{screen}");
    assert!(!screen.contains("hello"));
    harness.send(&keys::ctrl('n'));
    assert!(harness.screen().contains("hello"));

    let unnamed = self::harness(vec![make_session("a")]);
    unnamed.send(&keys::ctrl('n'));
    let expected = "No named sessions in current folder. Press ctrl+n to show all, or Tab to view all.";
    assert!(unnamed.screen().contains(expected), "{}", unnamed.screen());
}

#[test]
fn path_toggle_and_search() {
    let sessions = vec![session_with_message("a", "apple pie"), session_with_message("b", "banana split")];
    let harness = harness(sessions);
    harness.send(&keys::ctrl('p'));
    assert!(harness.screen().contains("ctrl+p path (on)"));
    assert!(selected_row(&harness).contains("/tmp/a.jsonl 1 "), "{}", harness.screen());
    harness.type_text("banana");
    let screen = harness.screen();
    assert!(screen.contains("banana split") && !screen.contains("apple pie"), "{screen}");
    for _ in 0..6 {
        harness.send(keys::BACKSPACE);
    }
    harness.type_text("re:^zzz");
    assert!(harness.screen().contains("No sessions in current folder. Press Tab to view all."));
}

#[test]
fn delete_confirmation_can_be_cancelled() {
    let harness = harness(vec![make_session("a")]);
    harness.send(&keys::ctrl('d'));
    assert!(harness.screen().contains("Delete session? enter confirm · escape/ctrl+c cancel"), "{}", harness.screen());
    harness.send("x");
    assert!(harness.screen().contains("Delete session?"));
    harness.send(keys::ESCAPE);
    let screen = harness.screen();
    assert!(!screen.contains("Delete session?") && screen.contains("ctrl+d delete"), "{screen}");
    assert!(selected_row(&harness).starts_with("› hello"));
}

#[test]
fn deleting_the_current_session_shows_an_error_that_expires() {
    let harness = Harness::new_shared(120, 40, |_| {
        let path = PathBuf::from("/tmp/a.jsonl");
        let options = SessionSelectorOptions::default();
        let loader = ready(vec![make_session("a")]);
        SessionSelector::new(loader, ready(Vec::new()), |_| {}, || {}, || {}, || {}, options, Some(&path))
    });
    harness.send(&keys::ctrl('d'));
    assert!(harness.screen().contains("Cannot delete the currently active session"));
    harness.run_for(Duration::from_millis(3100));
    assert!(!harness.screen().contains("Cannot delete"));
}

#[test]
fn load_errors_show_a_status_and_an_empty_list() {
    let failing: Loader = Box::new(|_, _| Box::pin(async { Err(anyhow::anyhow!("boom")) }));
    let harness = Harness::new_shared(120, 40, |tui| mount(tui, failing, ready(Vec::new()), Default::default()));
    let screen = harness.screen();
    assert!(screen.contains("Failed to load sessions: boom"), "{screen}");
    assert!(screen.contains("No sessions in current folder. Press Tab to view all."));
}

#[tokio::test(flavor = "current_thread")]
async fn progress_shows_partial_sessions_and_a_counter() {
    local(async {
        let (sender, current, _) = deferred(Some(vec![make_session("partial")]));
        let harness = Harness::new_shared(120, 40, |tui| mount(tui, current, ready(Vec::new()), Default::default()));
        let screen = harness.screen();
        assert!(screen.contains("○ Current Folder | Loading 1/2"), "{screen}");
        assert!(selected_row(&harness).starts_with("› hello"));
        sender.send(vec![session_with_message("done", "finished")]).unwrap();
        settle().await;
        harness.settle();
        let screen = harness.screen();
        assert!(screen.contains("◉ Current Folder") && screen.contains("finished"), "{screen}");
    })
    .await;
}

#[test]
fn rename_escape_and_failure() {
    let options = SessionSelectorOptions {
        rename_session: Some(Box::new(|_: PathBuf, _: String| Err(anyhow::anyhow!("nope")))),
        show_rename_hint: None,
    };
    let harness =
        Harness::new_shared(120, 40, |tui| mount(tui, ready(vec![make_session("a")]), ready(vec![]), options));
    assert!(harness.screen().contains("ctrl+r rename"));
    harness.send(&keys::ctrl('r'));
    let screen = harness.screen();
    assert!(
        screen.contains("Rename Session") && screen.contains("enter to save · escape/ctrl+c to cancel"),
        "{screen}"
    );
    harness.send(keys::ESCAPE);
    assert!(harness.screen().contains("Resume Session (Current Folder)"));
    harness.send(&keys::ctrl('r'));
    harness.send(keys::ENTER);
    assert!(harness.screen().contains("Rename Session"), "blank names are ignored");
    harness.type_text("new");
    harness.send(keys::ENTER);
    let screen = harness.screen();
    assert!(screen.contains("Failed to rename: nope") && screen.contains("Resume Session"), "{screen}");
}

fn write_session(dir: &Path, id: &str, cwd: &Path, message: &str, day: u32) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("2026-01-0{day}T00-00-00-000Z_{id}.jsonl"));
    let time = format!("2026-01-0{day}T00:00:00.000Z");
    let header =
        json!({"type": "session", "version": 3, "id": id, "timestamp": time, "cwd": cwd.display().to_string()});
    let entry = json!({
        "type": "message", "id": format!("{id}-1"), "parentId": null, "timestamp": time,
        "message": {"role": "user", "content": message},
    });
    std::fs::write(&path, format!("{header}\n{entry}\n")).unwrap();
    path
}

async fn wait_for(harness: &Selector, condition: impl Fn(&str) -> bool) -> String {
    for _ in 0..400 {
        settle().await;
        harness.settle();
        let screen = harness.screen();
        if condition(&screen) {
            return screen;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition not met:\n{}", harness.screen());
}

fn file_harness(root: &Path, rename: bool) -> Selector {
    let (agent, cwd) = (root.join("agent"), root.join("project"));
    let current = Box::new(project_sessions_loader(cwd, None, agent.clone()));
    let all = Box::new(all_sessions_loader(None, agent));
    let options = SessionSelectorOptions {
        rename_session: rename
            .then(|| Box::new(|path: PathBuf, name: String| rename_session(&path, &name)) as RenameSessionFn),
        show_rename_hint: None,
    };
    Harness::new_shared(120, 40, |tui| mount(tui, current, all, options))
}

#[tokio::test(flavor = "current_thread")]
async fn loaders_list_project_and_all_sessions() {
    local(async {
        let root = tempfile::tempdir().unwrap();
        let (agent, cwd, other) = (root.path().join("agent"), root.path().join("project"), root.path().join("other"));
        write_session(&session_dir(&agent, &cwd), "p1", &cwd, "project question", 1);
        write_session(&session_dir(&agent, &other), "o1", &other, "other question", 2);
        let harness = file_harness(root.path(), false);
        let screen = wait_for(&harness, |screen| screen.contains("project question")).await;
        assert!(!screen.contains("other question"), "{screen}");
        harness.send(keys::TAB);
        let screen = wait_for(&harness, |screen| screen.contains("◉ All") && screen.contains("other question")).await;
        assert!(screen.contains("project question"));
        assert!(screen.contains(&other.display().to_string()), "all scope shows each cwd: {screen}");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn delete_removes_the_file_and_reloads() {
    local(async {
        let root = tempfile::tempdir().unwrap();
        let (agent, cwd) = (root.path().join("agent"), root.path().join("project"));
        let dir = session_dir(&agent, &cwd);
        let older = write_session(&dir, "s1", &cwd, "keep me", 1);
        let newer = write_session(&dir, "s2", &cwd, "delete me", 2);
        let harness = file_harness(root.path(), false);
        wait_for(&harness, |screen| screen.contains("delete me")).await;
        assert!(selected_row(&harness).starts_with("› delete me"));
        harness.send(&keys::ctrl('d'));
        harness.send(keys::ENTER);
        let screen = wait_for(&harness, |screen| !screen.contains("delete me")).await;
        assert!(screen.contains("Session deleted") || screen.contains("Session moved to trash"), "{screen}");
        assert!(screen.contains("keep me"));
        assert!(!newer.exists() && older.exists());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn rename_writes_the_name_and_reloads() {
    local(async {
        let root = tempfile::tempdir().unwrap();
        let (agent, cwd) = (root.path().join("agent"), root.path().join("project"));
        write_session(&session_dir(&agent, &cwd), "s1", &cwd, "original question", 1);
        let harness = file_harness(root.path(), true);
        wait_for(&harness, |screen| screen.contains("original question")).await;
        harness.send(&keys::ctrl('r'));
        assert!(harness.screen().contains("Rename Session"));
        harness.type_text("Renamed");
        harness.send(keys::ENTER);
        let screen = wait_for(&harness, |screen| screen.contains("Resume Session") && screen.contains("Renamed")).await;
        assert!(!screen.contains("original question"), "{screen}");
    })
    .await;
}
