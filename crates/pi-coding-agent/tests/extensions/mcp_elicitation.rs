//! MCP elicitation (pi-rust): the dialog, and the `mcp_elicitation` and `mcp_elicitation_result`
//! events that can answer a request or replace the answer.

use serde_json::{Value, json};

use crate::harness::{Harness, Setup, eventually, harness};
use crate::mcp_fake::{Fake, FakeOptions};

const RECORDER: &str = r#"export default function (pi) {
	pi.on("mcp_elicitation", (event, ctx) => {
		ctx.ui.notify(`elicit ${JSON.stringify(event)}`);
		if (event.message === "auto") return { action: "accept", content: { name: "auto" } };
	});
	pi.on("mcp_elicitation_result", (event, ctx) => {
		ctx.ui.notify(`result ${JSON.stringify(event)}`);
		if (event.content?.name === "swap") return { action: "decline" };
	});
}
"#;

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "title": "Name" },
            "age": { "type": "integer" },
            "subscribe": { "type": "boolean", "description": "Get the newsletter", "default": false }
        },
        "required": ["name"]
    })
}

async fn mcp_harness(files: Vec<(&'static str, String)>) -> (Harness, Fake) {
    let fake = Fake::new(FakeOptions::default());
    let config = json!({ "url": "http://unused.invalid", "exposure": "direct" });
    let mcp = fake.session_options("forms", config, json!({}));
    let load = files.iter().map(|(path, _)| *path).collect();
    let h = harness(Setup { files, load, mcp: Some(mcp), ..Setup::default() }).await;
    eventually(|| h.session.all_tools().iter().any(|tool| tool.name == "mcp__forms__search")).await;
    (h, fake)
}

fn answers(h: &Harness, answers: &[&str]) {
    *h.ui.answers.lock().unwrap() = answers.iter().map(|answer| answer.to_string()).collect();
    h.ui.calls.lock().unwrap().clear();
}

async fn form(fake: &Fake, id: &str, message: &str) -> Value {
    let params = json!({ "mode": "form", "message": message, "requestedSchema": schema() });
    fake.request_client(id, "elicitation/create", params).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn asks_with_dialogs_and_emits_the_events() {
    let (h, fake) = mcp_harness(vec![("recorder.ts", RECORDER.to_string())]).await;

    answers(&h, &["Respond", "Ada", "x", "36", "Yes", "Accept"]);
    let answer = form(&fake, "f1", "Who are you?").await;
    assert_eq!(answer, json!({ "action": "accept", "content": { "name": "Ada", "age": 36, "subscribe": true } }));
    let elicit = json!({ "type": "mcp_elicitation", "server": "forms", "message": "Who are you?", "mode": "form",
        "requestedSchema": schema() });
    let result = json!({ "type": "mcp_elicitation_result", "server": "forms", "action": "accept", "mode": "form",
        "content": { "name": "Ada", "age": 36, "subscribe": true } });
    let header = "MCP server \"forms\" asks for information\n\nWho are you?";
    assert_eq!(
        h.ui.calls(),
        [
            format!("notify(info): elicit {elicit}"),
            format!("select: {header} [Respond, Decline]"),
            "input: Name".to_string(),
            "input: age (optional)".to_string(),
            "input: age (optional)\nInvalid value: A whole number.".to_string(),
            "select: subscribe (optional)\nGet the newsletter [No, Yes, Skip]".to_string(),
            format!("select: {header}\n\nname: Ada\nage: 36\nsubscribe: true [Accept, Decline]"),
            format!("notify(info): result {result}"),
        ]
    );

    // An `mcp_elicitation` answer skips the dialog and the result event.
    answers(&h, &[]);
    assert_eq!(form(&fake, "f2", "auto").await, json!({ "action": "accept", "content": { "name": "auto" } }));
    assert_eq!(h.ui.calls().len(), 1, "{:?}", h.ui.calls());

    // A result handler replaces the answer.
    answers(&h, &["Respond", "swap", "", "Skip", "Accept"]);
    assert_eq!(form(&fake, "f3", "Again?").await, json!({ "action": "decline" }));

    // Esc cancels.
    answers(&h, &[]);
    assert_eq!(form(&fake, "f4", "Later?").await, json!({ "action": "cancel" }));
    assert!(h.ui.calls().last().unwrap().contains(r#""action":"cancel""#), "{:?}", h.ui.calls());
    assert_eq!(h.errors(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_decline_of_any_handler_wins_and_invalid_content_declines() {
    let accept = r#"export default function (pi) {
	pi.on("mcp_elicitation", (event) =>
		event.message === "bad" ? { action: "accept", content: { age: "old" } } : { action: "accept", content: { name: "a" } });
}
"#;
    let decline = r#"export default function (pi) {
	pi.on("mcp_elicitation", (event) => (event.message === "vote" ? { action: "decline" } : undefined));
}
"#;
    let (h, fake) = mcp_harness(vec![("accept.ts", accept.into()), ("decline.ts", decline.into())]).await;
    assert_eq!(form(&fake, "v1", "vote").await, json!({ "action": "decline" }));
    assert_eq!(form(&fake, "v2", "other").await, json!({ "action": "accept", "content": { "name": "a" } }));
    assert_eq!(form(&fake, "v3", "bad").await, json!({ "action": "decline" }));
    assert!(h.ui.calls().is_empty(), "{:?}", h.ui.calls());
}

#[tokio::test(flavor = "multi_thread")]
async fn url_mode_shows_the_url_and_asks_for_consent() {
    let (h, fake) = mcp_harness(vec![("recorder.ts", RECORDER.to_string())]).await;
    answers(&h, &["Accept (I will open it myself)"]);
    let url = "https://xn--exmple-cua.com/connect";
    let params = json!({ "mode": "url", "message": "Connect your account.", "url": url, "elicitationId": "e1" });
    assert_eq!(fake.request_client("u1", "elicitation/create", params).await.unwrap(), json!({ "action": "accept" }));
    let calls = h.ui.calls();
    assert_eq!(
        calls[1],
        format!(
            "select: MCP server \"forms\" asks you to open a URL\n\nConnect your account.\n\n{url}\n\
             Domain: xn--exmple-cua.com\nWarning: the domain uses Punycode and can look like a different domain. \
             [Open in browser, Accept (I will open it myself), Decline]"
        )
    );
    let result = json!({ "type": "mcp_elicitation_result", "server": "forms", "action": "accept", "mode": "url", "elicitationId": "e1" });
    assert_eq!(calls[2], format!("notify(info): result {result}"));
    assert!(calls[0].contains(&format!(r#""url":"{url}","elicitationId":"e1""#)), "{}", calls[0]);
}
