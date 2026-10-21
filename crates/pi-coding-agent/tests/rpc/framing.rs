//! Framing and dispatch edge cases (upstream `rpc-jsonl.test.ts` and `rpc.md` errors).

use std::io::Write;
use std::time::Duration;

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::rpc::{RpcModeOptions, serve_rpc};
use pi_coding_agent::session::SessionManager;
use serde_json::json;

use crate::harness::{Options, harness};
use crate::support::start;

#[tokio::test]
async fn malformed_and_unknown_records_get_error_responses() {
    let mut rpc = start(Options::default()).await;
    rpc.write("not json\n\n").await;
    let parse = rpc.wait_for(|record| record["command"] == "parse").await;
    assert_eq!(parse.as_object().unwrap().keys().collect::<Vec<_>>(), ["type", "command", "success", "error"]);
    assert!(parse["error"].as_str().unwrap().starts_with("Failed to parse command: "));
    rpc.wait_for(|_| rpc.records_of("response").len() == 2).await;

    let unknown = rpc.request(json!({"id": "u", "type": "nope"})).await;
    assert_eq!(
        unknown,
        json!({"id": "u", "type": "response", "command": "nope", "success": false, "error": "Unknown command: nope"})
    );
    let untyped = rpc.request(json!({"id": "t"})).await;
    assert_eq!(
        untyped,
        json!({"id": "t", "type": "response", "success": false, "error": "Unknown command: undefined"})
    );
    let invalid = rpc.request(json!({"id": "m", "type": "set_model", "provider": "local"})).await;
    assert_eq!((invalid["command"].clone(), invalid["success"].clone()), (json!("set_model"), json!(false)));
    assert!(invalid["error"].as_str().unwrap().contains("modelId"), "{invalid}");
    rpc.send(json!({"id": 7, "type": "get_state"})).await;
    rpc.wait_for(|record| record["id"] == 7 && record["success"] == true).await;
    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test]
async fn accepts_crlf_partial_and_batched_records() {
    let mut rpc = start(Options::default()).await;
    rpc.write("{\"id\":\"crlf\",\"type\":\"get_state\"}\r\n").await;
    assert_eq!(rpc.response("crlf").await["success"], true);

    rpc.write("{\"id\":\"part\",\"type\":").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(rpc.responses("part").is_empty(), "no record before LF");
    rpc.write("\"get_state\"}\n").await;
    assert_eq!(rpc.response("part").await["command"], "get_state");

    rpc.write(
        "{\"id\":\"one\",\"type\":\"set_steering_mode\",\"mode\":\"all\"}\n{\"id\":\"two\",\"type\":\"get_state\"}\n",
    )
    .await;
    let state = rpc.response("two").await;
    assert_eq!(state["data"]["steeringMode"], "all", "records run in order");
    let ids: Vec<String> =
        rpc.records_of("response").iter().map(|record| record["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids, ["crlf", "part", "one", "two"]);

    rpc.write("{\"id\":\"last\",\"type\":\"get_state\"}").await;
    assert_eq!(rpc.finish().await, 0);
    assert_eq!(rpc.responses("last").len(), 1, "a final record without LF is handled at end of input");
}

#[tokio::test]
async fn unicode_separators_stay_inside_records() {
    let mut rpc = start(Options { responses: vec![FauxResponse::text("ok\u{2028}fine")], ..Options::default() }).await;
    rpc.data(json!({"id": "n", "type": "set_session_name", "name": "a\u{2028}b\u{2029}c"})).await;
    assert_eq!(rpc.data(json!({"id": "s", "type": "get_state"})).await["sessionName"], "a\u{2028}b\u{2029}c");
    rpc.data(json!({"id": "p", "type": "prompt", "message": "line\u{2028}separator"})).await;
    rpc.settled(1).await;
    let request = serde_json::to_string(&rpc.h.requests()[0].messages).unwrap();
    assert!(request.contains("line\u{2028}separator"));
    assert!(rpc.lines().iter().any(|line| line.contains("ok\u{2028}fine")), "not escaped on output");
    rpc.finish().await;
}

#[tokio::test]
async fn end_of_input_aborts_the_run_and_disposes() {
    let mut rpc = start(Options { responses: vec![FauxResponse::Hang], ..Options::default() }).await;
    rpc.data(json!({"id": "p", "type": "prompt", "message": "Forever"})).await;
    rpc.wait_for(|record| record["type"] == "agent_start").await;
    let session = rpc.runtime.session();
    assert_eq!(rpc.finish().await, 0);
    assert!(!session.is_streaming());
}

struct Broken;

impl Write for Broken {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_closed_output_stops_rpc_mode_with_exit_code_1() {
    let h = harness(Options::default()).await;
    let manager = SessionManager::create(&h.cwd, Some(&h.agent_dir.join("sessions")));
    let runtime = std::sync::Arc::new(crate::offline_guard::app_runtime(h.app_options(), manager).await);
    let (mut client, server) = tokio::io::duplex(1024);
    let task = tokio::spawn(serve_rpc(runtime, server, Box::new(Broken), RpcModeOptions::default()));
    tokio::io::AsyncWriteExt::write_all(&mut client, b"{\"type\":\"get_state\"}\n").await.unwrap();
    let code = tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap();
    assert_eq!(code, 1);
}
