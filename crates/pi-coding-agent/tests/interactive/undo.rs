//! `/undo` (pi-rust extension): restore the files the edit and write tools changed in the last
//! prompt, over the faux provider.

use std::path::PathBuf;

use pi_ai::providers::faux::FauxResponse;
use serde_json::json;

use crate::harness::{Options, Ui, local, start};

fn write(id: &str, path: &str, content: &str) -> (String, &'static str, serde_json::Value) {
    (id.to_string(), "write", json!({"path": path, "content": content}))
}

fn edit(id: &str, path: &str, old: &str, new: &str) -> (String, &'static str, serde_json::Value) {
    (id.to_string(), "edit", json!({"path": path, "edits": [{"oldText": old, "newText": new}]}))
}

fn calls(calls: &[(String, &'static str, serde_json::Value)]) -> FauxResponse {
    let calls: Vec<(&str, &str, serde_json::Value)> =
        calls.iter().map(|(id, name, args)| (id.as_str(), *name, args.clone())).collect();
    FauxResponse::tool_calls(&calls)
}

fn file(ui: &Ui, name: &str) -> PathBuf {
    ui.session().cwd().join(name)
}

fn read(ui: &Ui, name: &str) -> Option<String> {
    std::fs::read_to_string(file(ui, name)).ok()
}

/// Submits `prompt` and waits until the run that ends with `reply` settles.
async fn run_prompt(ui: &Ui, prompt: &str, reply: &str) {
    ui.submit(prompt).await;
    ui.wait_for(reply).await;
    ui.wait_until("the run to settle", |ui| !ui.session().is_streaming()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_restores_written_and_edited_files_and_deletes_created_ones() {
    local(async {
        let responses = vec![
            calls(&[
                write("call-1", "existing.txt", "rewritten\n"),
                edit("call-2", "notes.txt", "alpha", "ALPHA"),
                write("call-3", "sub/new.txt", "fresh\n"),
            ]),
            FauxResponse::text("changes-made"),
        ];
        let files = vec![("existing.txt", "original\n"), ("notes.txt", "alpha\nbeta\n")];
        let ui = start(Options { responses, files, ..Options::default() }).await;
        run_prompt(&ui, "change the files", "changes-made").await;
        assert_eq!(read(&ui, "existing.txt").as_deref(), Some("rewritten\n"));
        assert_eq!(read(&ui, "notes.txt").as_deref(), Some("ALPHA\nbeta\n"));
        assert_eq!(read(&ui, "sub/new.txt").as_deref(), Some("fresh\n"));

        ui.submit("/undo").await;
        ui.wait_for("Undid the file changes").await;
        assert_eq!(read(&ui, "existing.txt").as_deref(), Some("original\n"));
        assert_eq!(read(&ui, "notes.txt").as_deref(), Some("alpha\nbeta\n"));
        assert!(!file(&ui, "sub/new.txt").exists());
        let history = ui.history();
        assert!(history.contains("restored  existing.txt"), "{history}");
        assert!(history.contains("restored  notes.txt"), "{history}");
        assert!(history.contains("deleted   sub/new.txt"), "{history}");
        assert!(history.contains("Bash tool changes are not tracked"), "{history}");
        assert!(history.contains("/tree"), "{history}");
        assert_eq!(ui.provider.requests().len(), 2, "/undo does not prompt the model");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_skips_files_changed_after_the_agent_wrote_them() {
    local(async {
        let responses = vec![
            calls(&[write("call-1", "mine.txt", "agent\n"), edit("call-2", "kept.txt", "one", "two")]),
            FauxResponse::text("edits-done"),
        ];
        let files = vec![("mine.txt", "start\n"), ("kept.txt", "one\n")];
        let ui = start(Options { responses, files, ..Options::default() }).await;
        run_prompt(&ui, "edit both", "edits-done").await;
        std::fs::write(file(&ui, "mine.txt"), "user edit\n").unwrap();

        ui.submit("/undo").await;
        ui.wait_for("Undid the file changes").await;
        assert_eq!(read(&ui, "mine.txt").as_deref(), Some("user edit\n"));
        assert_eq!(read(&ui, "kept.txt").as_deref(), Some("one\n"));
        let history = ui.history();
        assert!(history.contains("skipped   mine.txt (changed since the agent wrote it)"), "{history}");
        assert!(history.contains("restored  kept.txt"), "{history}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_goes_back_one_prompt_at_a_time() {
    local(async {
        let responses = vec![
            calls(&[write("call-1", "a.txt", "v1\n")]),
            FauxResponse::text("first-done"),
            calls(&[edit("call-2", "a.txt", "v1", "v2"), write("call-3", "b.txt", "created\n")]),
            FauxResponse::text("second-done"),
        ];
        let ui = start(Options { responses, files: vec![("a.txt", "v0\n")], ..Options::default() }).await;
        run_prompt(&ui, "first prompt", "first-done").await;
        run_prompt(&ui, "second prompt", "second-done").await;
        assert_eq!(read(&ui, "a.txt").as_deref(), Some("v2\n"));
        assert_eq!(ui.session().undoable_prompts(), 2);

        ui.submit("/undo").await;
        ui.wait_for("1 earlier prompt has changes").await;
        assert_eq!(read(&ui, "a.txt").as_deref(), Some("v1\n"));
        assert!(!file(&ui, "b.txt").exists());

        ui.submit("/undo").await;
        ui.wait_until("the second undo", |ui| read(ui, "a.txt").as_deref() == Some("v0\n")).await;
        assert_eq!(ui.session().undoable_prompts(), 0);

        ui.submit("/undo").await;
        ui.wait_for("Nothing to undo").await;
        assert_eq!(read(&ui, "a.txt").as_deref(), Some("v0\n"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_waits_for_the_run_and_ignores_other_sessions() {
    local(async {
        let responses =
            vec![calls(&[write("call-1", "a.txt", "agent\n")]), FauxResponse::text("write-done"), FauxResponse::Hang];
        let ui = start(Options { responses, ..Options::default() }).await;
        run_prompt(&ui, "write it", "write-done").await;

        ui.submit("keep going").await;
        ui.wait_until("streaming", |ui| ui.session().is_streaming()).await;
        ui.submit("/undo").await;
        ui.wait_for("Wait for the current response to finish").await;
        assert_eq!(read(&ui, "a.txt").as_deref(), Some("agent\n"));
        ui.send("\x1b").await;
        ui.wait_until("the abort", |ui| !ui.session().is_streaming()).await;

        ui.submit("/new").await;
        ui.wait_for("New session started").await;
        ui.submit("/undo").await;
        ui.wait_for("Nothing to undo").await;
        assert_eq!(read(&ui, "a.txt").as_deref(), Some("agent\n"), "checkpoints stay with their session");
    })
    .await;
}
