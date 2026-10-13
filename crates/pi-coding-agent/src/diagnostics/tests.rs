//! Bug report contents and the summary request (upstream `bug-report.test.ts`).

use std::collections::BTreeMap;
use std::sync::Arc;

use pi_agent::StreamFn;
use pi_ai::providers::faux::{FauxProvider, FauxResponse};
use pi_ai::{ApiProvider, AssistantMessage, ContentBlock, Message, StopReason, UserContent, UserMessage};
use serde_json::{Value, json};

use super::bug_summary::bug_summary_prompt;
use super::zip::read_zip_archive;
use super::*;
use crate::session::SessionManager;

fn user(text: &str) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: 1 })
}

fn assistant(stop: StopReason, error: Option<&str>, diagnostics: Vec<Value>) -> Message {
    let mut message = AssistantMessage::empty(&FauxProvider::model());
    message.content = vec![ContentBlock::text("partial")];
    message.stop_reason = stop;
    message.error_message = error.map(String::from);
    message.diagnostics = diagnostics;
    Message::Assistant(message)
}

#[test]
fn metadata_redacts_settings_and_describes_the_model() {
    let mut model = FauxProvider::model();
    model.base_url = "https://user:pw@api.example/v1?token=abc".into();
    model.headers.insert("X-Key".into(), "secret".into());
    model.compat = Some(json!({"apiKey": "k", "maxTokensField": "max_tokens"}));
    let environment = BTreeMap::from([
        ("PI_OFFLINE".to_string(), "1".to_string()),
        ("PI_CODING_AGENT_DIR".to_string(), "/secret/path".to_string()),
        ("SHELL".to_string(), "/usr/bin/zsh".to_string()),
        ("TMUX".to_string(), "/tmp/tmux".to_string()),
    ]);
    let options = BugReportMetadataOptions {
        id: Some("report-1".into()),
        hint: Some("  it broke  ".into()),
        session_id: "s1".into(),
        cwd: "/work".into(),
        include_session: true,
        message_count: 3,
        model: Some(model),
        thinking_level: "medium".into(),
        extensions: vec![ExtensionDescription {
            path: "/ext/a.ts".into(),
            source: "git:https://pat@github.com/o/r".into(),
            scope: "user".into(),
            origin: "package".into(),
            hidden: false,
        }],
        extension_errors: vec![("/ext/b.ts".into(), "SyntaxError".into())],
        global_settings: json!({"trackingId": "t", "deviceId": "d", "httpProxy": "http://u:p@proxy:1/", "theme": "dark"}),
        project_settings: json!({"compaction": {"reserveTokens": 100}}),
        environment,
        ..BugReportMetadataOptions::default()
    };
    let metadata = collect_bug_report_metadata(&options);
    assert_eq!(metadata["schemaVersion"], 1);
    assert_eq!(metadata["id"], "report-1");
    assert_eq!(metadata["hint"], "it broke");
    assert_eq!(
        metadata["session"],
        json!({"id": "s1", "included": true, "summaryIncluded": false, "messageCount": 3, "cwd": "/work"})
    );
    assert_eq!(metadata["model"]["baseUrl"], "https://api.example/v1?token=%3Credacted%3E");
    assert_eq!(metadata["model"]["headerNames"], json!(["X-Key"]));
    assert_eq!(metadata["model"]["compat"], json!({"apiKey": "<redacted>", "maxTokensField": "max_tokens"}));
    assert_eq!(metadata["provider"], Value::Null);
    assert_eq!(metadata["extensions"][0]["source"], "git:https://github.com/o/r");
    assert_eq!(metadata["extensionErrors"], json!([{"path": "/ext/b.ts", "error": "SyntaxError"}]));
    assert_eq!(metadata["settings"]["global"], json!({"httpProxy": "http://proxy:1/", "theme": "dark"}));
    let environment = &metadata["environment"];
    assert_eq!(environment["piEnvironmentVariables"], json!(["PI_CODING_AGENT_DIR", "PI_OFFLINE"]));
    assert_eq!((environment["shell"].as_str(), environment["terminal"]["tmux"].as_bool()), (Some("zsh"), Some(true)));
    assert!(!metadata.to_string().contains("/secret/path"));
    assert!(metadata["createdAt"].as_str().unwrap().ends_with('Z'));

    let without = collect_bug_report_metadata(&BugReportMetadataOptions { hint: Some("  ".into()), ..options });
    assert_eq!(without["hint"], Value::Null);
}

#[test]
fn diagnostics_list_failed_turns_and_crashes_only() {
    let mut session = SessionManager::in_memory(std::path::Path::new("/work"));
    session.append_message(&user("hi")).unwrap();
    session.append_message(&assistant(StopReason::Stop, None, Vec::new())).unwrap();
    let failed = session.append_message(&assistant(StopReason::Error, Some("429 rate limited"), Vec::new())).unwrap();
    session.append_message(&assistant(StopReason::Stop, None, vec![json!({"kind": "retry"})])).unwrap();
    let crash = CrashRecord {
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        version: "0.1.0".into(),
        kind: "fatal_error".into(),
        message: "panic".into(),
        stack: None,
        session_file: None,
        cwd: "/".into(),
        notified: Some(true),
    };
    let diagnostics = collect_bug_report_diagnostics(session.session_id(), session.entries(), &[crash]);
    assert_eq!(diagnostics["entryCount"], session.entries().len());
    assert_eq!(diagnostics["assistantMessageCount"], 3);
    let turns = diagnostics["assistant"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0]["entryId"], failed);
    assert_eq!(turns[0]["stopReason"], "error");
    assert_eq!(turns[0]["errorMessage"], "429 rate limited");
    assert_eq!(turns[1]["diagnostics"], json!([{"kind": "retry"}]));
    assert!(diagnostics["crashes"][0].get("notified").is_none());
    assert!(!diagnostics.to_string().contains("partial"));
}

#[test]
fn files_and_archives_hold_the_bundle() {
    let bundle = BugReportBundle {
        metadata: json!({"id": "r1", "createdAt": "2026-01-01T00:00:00.000Z", "hint": "h",
            "session": {"included": false, "summaryIncluded": true}}),
        diagnostics: json!({"assistant": []}),
        session_jsonl: None,
        summary: Some("## Summary".into()),
    };
    let files = bug_report_files(&bundle);
    let names: Vec<&str> = files.iter().map(|file| file.name).collect();
    assert_eq!(names, ["report.json", "diagnostics.json", "summary.md"]);
    assert_eq!(
        files[0].data,
        "{\n  \"id\": \"r1\",\n  \"createdAt\": \"2026-01-01T00:00:00.000Z\",\n  \"hint\": \"h\",\n  \"session\": {\n    \"included\": false,\n    \"summaryIncluded\": true\n  }\n}\n"
    );
    assert_eq!(files[2].data, "## Summary\n");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(bug_report_archive_file_name("r1"));
    assert!(path.ends_with("pi-bug-report-r1.zip"));
    write_bug_report_archive(&bundle, &path).unwrap();
    let entries = read_zip_archive(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[2].data, b"## Summary\n");

    let entry = BugReportSessionEntryData::from_bundle(&bundle, "zip", Some(path.display().to_string()));
    let value = serde_json::to_value(&entry).unwrap();
    assert_eq!(value["sessionIncluded"], false);
    assert_eq!(value["summaryIncluded"], true);
    assert_eq!(value["delivery"], "zip");
    assert_eq!(value["createdAt"], "2026-01-01T00:00:00.000Z");
}

fn stream_fn(responses: Vec<FauxResponse>) -> (StreamFn, Arc<FauxProvider>) {
    let provider = FauxProvider::new(responses);
    let faux = provider.clone();
    (Arc::new(move |model, context, options| faux.stream(model, context, options)), provider)
}

#[tokio::test]
async fn summaries_use_the_session_model_and_reject_bad_output() {
    let model = FauxProvider::model();
    let messages = vec![user("run the tests"), assistant(StopReason::Stop, None, Vec::new())];
    let (stream, provider) = stream_fn(vec![
        FauxResponse::text("  ## What went wrong\nTests hang.  "),
        FauxResponse::tool_call("t", "bash", json!({})),
    ]);
    let options = || BugReportSummaryOptions {
        messages: &messages,
        hint: Some("tests hang"),
        model: Some(&model),
        stream_fn: Some(&stream),
        ..BugReportSummaryOptions::default()
    };
    assert_eq!(generate_bug_report_summary(options()).await.unwrap(), "## What went wrong\nTests hang.");
    let request = &provider.requests()[0];
    assert!(request.system_prompt.as_deref().unwrap().starts_with("You are helping a user file a bug report"));
    let prompt = format!("{:?}", request.messages[0]);
    assert!(prompt.contains("<user-report>\\ntests hang\\n</user-report>"), "{prompt}");
    let error = generate_bug_report_summary(options()).await.unwrap_err();
    assert_eq!(error, "Bug report summary attempted to call a tool");
}

#[test]
fn long_transcripts_keep_the_newest_messages() {
    let messages: Vec<Message> = (0..50).map(|index| user(&format!("{index} {}", "word ".repeat(400)))).collect();
    let prompt = bug_summary_prompt(&messages, None, 4_000);
    assert!(prompt.starts_with("Note: only the last "), "{}", &prompt[..80]);
    assert!(prompt.contains("49 word"));
    assert!(!prompt.contains("<user-report>"));
    assert!(prompt.ends_with("Keep the report factual and concise."));
}
