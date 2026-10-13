//! Port of upstream `test/node.test.ts` for the Rust file, HTTP, and environment host adapters.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use pi_embedded::{
    CredentialStore, EnvironmentCredentials, FileSessionStore, FsWorkspace, HttpNetworkAdapter, InvocationPhase,
    NetworkAdapter, NetworkRequest, Reconciliation, SessionSnapshot, SessionStatus, SessionStore, ToolEffect,
    ToolInvocation, ToolResult, WorkspaceAdapter,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

fn snapshot(id: &str, revision: u64) -> SessionSnapshot {
    serde_json::from_value(json!({
        "version": 1, "id": id, "revision": revision, "sequence": 0, "status": "idle",
        "model": {"id": "test", "name": "Test", "api": "openai-completions", "provider": "openai",
            "baseUrl": "https://example.test", "reasoning": false, "input": ["text"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 8192, "maxTokens": 1024},
        "messages": [], "steering": [], "followUps": [], "invocations": [], "usage": [],
        "contextStart": 0, "needsResponse": false
    }))
    .unwrap()
}

fn invocation(id: &str) -> ToolInvocation {
    ToolInvocation {
        session_id: "test".into(),
        id: id.into(),
        name: "workspace_edit".into(),
        args: Default::default(),
        effect: ToolEffect::Mutation,
        phase: InvocationPhase::Uncertain,
        result: None,
        approved: None,
    }
}

fn sha(value: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, value.as_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn token() -> CancellationToken {
    CancellationToken::new()
}

#[tokio::test]
async fn file_store_persists_arbitrary_ids_and_serializes_compare_and_swap_across_instances() {
    let root = tempfile::tempdir().unwrap();
    let (first, second) = (FileSessionStore::new(root.path()), FileSessionStore::new(root.path()));
    let id = "../../outside/session";
    assert!(first.load(id).await.unwrap().is_none());
    first.save(&snapshot(id, 0), None).await.unwrap();
    let next = snapshot(id, 1);
    let (a, b) = tokio::join!(first.save(&next, Some(0)), second.save(&next, Some(0)));
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(), 1);
    assert_eq!(second.load(id).await.unwrap(), Some(snapshot(id, 1)));
    let listed = first.list().await.unwrap();
    assert_eq!(
        (listed.len(), listed[0].id.as_str(), listed[0].status, listed[0].revision),
        (1, id, SessionStatus::Idle, 1)
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    let conflict = second.save(&snapshot(id, 2), None).await.unwrap_err();
    assert!(conflict.to_string().contains("revision conflict"));
}

#[tokio::test]
async fn file_store_rejects_corrupt_checkpoints_instead_of_losing_sessions() {
    let root = tempfile::tempdir().unwrap();
    let store = FileSessionStore::new(root.path());
    store.save(&snapshot("corrupt", 0), None).await.unwrap();
    let file = std::fs::read_dir(root.path()).unwrap().next().unwrap().unwrap().path();
    std::fs::write(file, "broken").unwrap();
    assert!(store.load("corrupt").await.is_err());
    assert!(store.list().await.is_err());
}

#[tokio::test]
async fn workspace_reads_searches_edits_with_revision_checks_and_restores_completed_receipts() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.txt"), "first\nneedle\n").unwrap();
    let workspace = FsWorkspace::new(root.path());
    let before = workspace.read("file.txt", token()).await.unwrap();
    let found = serde_json::to_value(workspace.search("needle", token()).await.unwrap()).unwrap();
    assert_eq!(found, json!([{"path": "file.txt", "line": 2, "text": "needle"}]));
    let after = workspace.edit("file.txt", &before.revision, "changed", "edit-1", token()).await.unwrap();
    assert_eq!(after.text, "changed");
    let conflict = workspace.edit("file.txt", &before.revision, "other", "edit-2", token()).await.unwrap_err();
    assert!(conflict.to_string().contains("revision conflict"));
    assert_eq!(workspace.edit("file.txt", &before.revision, "changed", "edit-1", token()).await.unwrap(), after);
    let reused = workspace.edit("file.txt", &before.revision, "other", "edit-1", token()).await.unwrap_err();
    assert!(reused.to_string().contains("Invocation ID reused"));
    let restored = FsWorkspace::new(root.path());
    let expected = ToolResult::text(serde_json::to_string(&after).unwrap(), serde_json::to_value(&after).unwrap());
    let reconciled = restored.reconcile_edit(invocation("edit-1"), token()).await.unwrap();
    assert_eq!(reconciled, Reconciliation::Completed { result: expected });
    assert_eq!(restored.reconcile_edit(invocation("absent"), token()).await.unwrap(), Reconciliation::NotApplied);
}

#[tokio::test]
async fn workspace_serializes_edits_sharing_a_workspace() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.txt"), "before").unwrap();
    let (first, second) = (FsWorkspace::new(root.path()), FsWorkspace::new(root.path()));
    let before = first.read("file.txt", token()).await.unwrap();
    let (a, b) = tokio::join!(
        first.edit("file.txt", &before.revision, "first", "first", token()),
        second.edit("file.txt", &before.revision, "second", "second", token()),
    );
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(), 1);
}

fn mark_pending(root: &std::path::Path, id: &str) -> (std::path::PathBuf, serde_json::Value) {
    let path = root.join(".pi-embedded-receipts").join(format!("{}.json", sha(id)));
    let mut receipt: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    receipt["phase"] = json!("pending");
    std::fs::write(&path, receipt.to_string()).unwrap();
    (path, receipt)
}

#[tokio::test]
async fn workspace_reconciles_a_pending_receipt_against_applied_unchanged_and_uncertain_contents() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("file.txt");
    std::fs::write(&file, "before").unwrap();
    let workspace = FsWorkspace::new(root.path());
    let before = workspace.read("file.txt", token()).await.unwrap();
    workspace.edit("file.txt", &before.revision, "after", "crash", token()).await.unwrap();
    let (receipt_path, receipt) = mark_pending(root.path(), "crash");
    assert!(matches!(
        workspace.reconcile_edit(invocation("crash"), token()).await.unwrap(),
        Reconciliation::Completed { .. }
    ));
    std::fs::write(&receipt_path, receipt.to_string()).unwrap();
    std::fs::write(&file, "external edit").unwrap();
    assert_eq!(workspace.reconcile_edit(invocation("crash"), token()).await.unwrap(), Reconciliation::Unknown);
    std::fs::write(&file, "before").unwrap();
    assert_eq!(workspace.reconcile_edit(invocation("crash"), token()).await.unwrap(), Reconciliation::NotApplied);
    let retried = workspace.edit("file.txt", &before.revision, "retry", "crash", token()).await.unwrap();
    assert_eq!(retried.text, "retry");
}

#[tokio::test]
async fn workspace_retries_pending_receipts_against_the_reviewed_revision_and_preserves_external_changes() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("file.txt");
    std::fs::write(&file, "before").unwrap();
    let workspace = FsWorkspace::new(root.path());
    let before = workspace.read("file.txt", token()).await.unwrap();
    let after = workspace.edit("file.txt", &before.revision, "after", "retry", token()).await.unwrap();
    let (receipt_path, receipt) = mark_pending(root.path(), "retry");
    let phase = || {
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&receipt_path).unwrap()).unwrap()["phase"]
            .clone()
    };
    std::fs::write(&file, "before").unwrap();
    assert_eq!(workspace.edit("file.txt", &before.revision, "after", "retry", token()).await.unwrap(), after);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
    assert_eq!(phase(), "completed");
    std::fs::write(&receipt_path, receipt.to_string()).unwrap();
    std::fs::write(&file, "external edit").unwrap();
    let error = workspace.edit("file.txt", &before.revision, "after", "retry", token()).await.unwrap_err();
    assert!(error.to_string().contains("requires reconciliation"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "external edit");
    std::fs::write(&file, "after").unwrap();
    assert_eq!(workspace.edit("file.txt", &before.revision, "after", "retry", token()).await.unwrap(), after);
    assert_eq!(phase(), "completed");
}

#[tokio::test]
async fn workspace_rejects_traversal_and_symlink_escapes_and_honors_cancellation() {
    let (root, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(outside.path().join("secret"), "private").unwrap();
    let workspace = FsWorkspace::new(root.path());
    let escape = workspace.read("../secret", token()).await.unwrap_err();
    assert!(escape.to_string().contains("escapes root"));
    let absolute = outside.path().join("secret").to_string_lossy().into_owned();
    assert!(workspace.read(&absolute, token()).await.unwrap_err().to_string().contains("escapes root"));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let linked = workspace.read("link/secret", token()).await.unwrap_err();
        assert!(linked.to_string().contains("symlink rejected"));
        assert!(workspace.search("private", token()).await.unwrap().is_empty());
    }
    let aborted = CancellationToken::new();
    aborted.cancel();
    assert!(workspace.search("x", aborted.clone()).await.unwrap_err().to_string().contains("cancelled"));
    assert!(workspace.read("file", aborted).await.unwrap_err().to_string().contains("cancelled"));
}

/// Serve one HTTP response: `head` lines, then each body chunk with chunked encoding. With
/// `hold`, keep the connection open after the chunks and report when the client closes it.
async fn serve(status: u16, chunks: Vec<Vec<u8>>, hold: bool) -> (String, tokio::sync::oneshot::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (closed, notice) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0u8; 4096];
        let _ = socket.read(&mut request).await;
        let head = format!(
            "HTTP/1.1 {status} Status\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        for chunk in chunks {
            socket.write_all(format!("{:x}\r\n", chunk.len()).as_bytes()).await.unwrap();
            socket.write_all(&chunk).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        if hold {
            let mut buffer = [0u8; 64];
            while socket.read(&mut buffer).await.is_ok_and(|read| read > 0) {}
        } else {
            socket.write_all(b"0\r\n\r\n").await.unwrap();
        }
        let _ = closed.send(());
    });
    (url, notice)
}

fn request(url: &str) -> NetworkRequest {
    NetworkRequest { url: url.into(), method: "POST".into(), headers: Default::default(), body: "{}".into() }
}

#[tokio::test]
async fn http_decodes_split_utf8_and_preserves_http_status() {
    let bytes = "a😀b".as_bytes().to_vec();
    let (url, _) = serve(429, vec![bytes[..3].to_vec(), bytes[3..].to_vec()], false).await;
    let response = HttpNetworkAdapter::new().request(request(&url), token()).await.unwrap();
    assert_eq!(response.status, 429);
    let body: Vec<String> = response.body.map(|part| part.unwrap()).collect().await;
    assert_eq!(body.concat(), "a😀b");
}

#[tokio::test]
async fn http_closes_response_streams_when_consumers_stop_and_on_cancellation() {
    let (url, closed) = serve(200, vec![b"data: first\n\n".to_vec()], true).await;
    let mut response = HttpNetworkAdapter::new().request(request(&url), token()).await.unwrap();
    assert_eq!(response.body.next().await.unwrap().unwrap(), "data: first\n\n");
    drop(response);
    tokio::time::timeout(std::time::Duration::from_secs(5), closed).await.unwrap().unwrap();

    let (url, closed) = serve(200, vec![b"first".to_vec()], true).await;
    let cancel = token();
    let mut response = HttpNetworkAdapter::new().request(request(&url), cancel.clone()).await.unwrap();
    cancel.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), closed).await.unwrap().unwrap();
    let mut failed = false;
    while let Some(item) = response.body.next().await {
        failed |= item.is_err_and(|error| error.to_string().contains("cancelled"));
    }
    assert!(failed, "a cancelled read reports the cancellation");
}

#[tokio::test]
async fn http_closes_a_body_returned_before_its_first_chunk() {
    let (url, closed) = serve(401, Vec::new(), true).await;
    let response = HttpNetworkAdapter::new().request(request(&url), token()).await.unwrap();
    assert_eq!(response.status, 401);
    drop(response.body);
    tokio::time::timeout(std::time::Duration::from_secs(5), closed).await.unwrap().unwrap();
}

#[tokio::test]
async fn http_propagates_transport_failures() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    assert!(HttpNetworkAdapter::new().request(request(&url), token()).await.is_err());
    let cancelled = token();
    cancelled.cancel();
    assert!(HttpNetworkAdapter::new().request(request(&url), cancelled).await.is_err());
}

#[tokio::test]
async fn keeps_credentials_in_the_applications_environment() {
    let variables = HashMap::from([("openai".to_string(), "APP_SECRET".to_string())]);
    let credentials = Arc::new(EnvironmentCredentials::with_environment(variables, HashMap::new()));
    credentials.set("openai", Some("secret".into())).await.unwrap();
    assert_eq!(credentials.get("openai").await.unwrap().as_deref(), Some("secret"));
    assert_eq!(credentials.variable("APP_SECRET").as_deref(), Some("secret"));
    credentials.set("openai", None).await.unwrap();
    assert_eq!(credentials.get("openai").await.unwrap(), None);
    let error = credentials.set("unknown", Some("secret".into())).await.unwrap_err();
    assert!(error.to_string().contains("No credential environment variable"));
}
