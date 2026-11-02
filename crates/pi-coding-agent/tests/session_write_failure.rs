//! Ported from upstream `test/session-manager/file-operations.test.ts` (#10166, #10661, #11460): a
//! failed write must not leave later entries pointing to a missing parent.
#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use common::*;
use pi_coding_agent::session::SessionManager;

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn entry_ids(session: &SessionManager) -> Vec<String> {
    session.entries().iter().map(|entry| entry.id().to_string()).collect()
}

#[test]
fn does_not_keep_an_entry_in_memory_when_writing_it_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), Some(dir.path()));
    let first_id = session.append_message(&user("first question")).unwrap();
    let file = session.session_file().unwrap().to_path_buf();

    chmod(&file, 0o444);
    assert!(session.append_message(&assistant("lost answer")).is_err());
    assert_eq!(session.leaf_id(), Some(first_id.as_str()));
    assert_eq!(entry_ids(&session).as_slice(), std::slice::from_ref(&first_id));

    chmod(&file, 0o644);
    let second_id = session.append_message(&assistant("second answer")).unwrap();
    assert_eq!(session.get_entry(&second_id).and_then(|entry| entry.parent_id()), Some(first_id.as_str()));

    let reopened = SessionManager::open_with(&file, Some(dir.path()), None).unwrap();
    assert_eq!(ids(&reopened.get_branch(None)), [first_id, second_id]);
}

#[test]
fn does_not_keep_an_entry_in_memory_when_creating_the_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir(&session_dir).unwrap();
    let mut session = SessionManager::create(dir.path(), Some(&session_dir));
    let model_change_id = session.append_model_change("anthropic", "claude-sonnet-4-5").unwrap();

    chmod(&session_dir, 0o555);
    let failed = session.append_message(&user("first question"));
    chmod(&session_dir, 0o755);
    assert!(failed.is_err());
    assert_eq!(session.leaf_id(), Some(model_change_id.as_str()));
    assert_eq!(entry_ids(&session), [model_change_id]);

    session.append_message(&user("second question")).unwrap();
    assert_eq!(file_roles(session.session_file().unwrap()), ["session", "model_change", "user"]);
}
