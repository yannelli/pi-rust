//! Port of upstream `llama-cpp-classify.test.ts` against a path-routed fake llama-server.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::llama_render::{
    answer_from_probabilities, label_probabilities, llama_server_root, peak_confidence, render_question,
};
use super::test_support::{answers_json, classifier_model, context};
use super::{ClassifierAnswer, ClassifierResult, LlamaCppClassify};
use super::{ClassifierApi, ClassifierContext, ClassifierOptions, ClassifierQuestion, ClassifierStopReason};
use crate::providers::mock_server::{MockResponse, MockServer, RecordedRequest};
use crate::registry::ProviderResponse;
use crate::request::RequestOptions;
use crate::types::Model;

type NextFn = Arc<dyn Fn(&str, u64) -> Vec<(&'static str, f64)> + Send + Sync>;

/// Behavior of the fake server; `next` gives the next-token log-probabilities by token text.
#[derive(Clone)]
struct Fake {
    next: NextFn,
    template: Option<fn(&[Value]) -> String>,
    tokenize: fn(&str) -> Vec<i64>,
}

impl Default for Fake {
    fn default() -> Self {
        Self { next: Arc::new(|_, _| vec![("A", -0.1), ("B", -2.5)]), template: None, tokenize: char_tokens }
    }
}

/// Token IDs: one per character, the character code.
fn char_tokens(content: &str) -> Vec<i64> {
    content.chars().map(|character| character as i64).collect()
}

/// Maps the multi-character labels `Yes` and `No` to single tokens, as a real vocabulary would.
fn word_tokens(content: &str) -> Vec<i64> {
    let mut tokens = Vec::new();
    for (index, part) in content.split('\n').enumerate() {
        if index > 0 {
            tokens.push('\n' as i64);
        }
        match part {
            "Yes" => tokens.push(89),
            "No" => tokens.push(78),
            _ => tokens.extend(char_tokens(part)),
        }
    }
    tokens
}

fn default_template(messages: &[Value]) -> String {
    let rendered: String = messages
        .iter()
        .map(|message| format!("<|{}|>\n{}\n", message["role"].as_str().unwrap(), message["content"].as_str().unwrap()))
        .collect();
    format!("{rendered}<|assistant|>\n")
}

async fn fake_server(fake: Fake) -> MockServer {
    MockServer::routed(move |request| {
        let (path, body) = (request.path.as_str(), &request.body);
        if path.ends_with("/tokenize") {
            return MockResponse::json(&json!({ "tokens": (fake.tokenize)(body["content"].as_str().unwrap()) }));
        }
        if path.ends_with("/apply-template") {
            let messages = body["messages"].as_array().unwrap();
            let prompt = fake.template.map_or_else(|| default_template(messages), |template| template(messages));
            return MockResponse::json(&json!({ "prompt": prompt }));
        }
        if path.ends_with("/completion") {
            let next = (fake.next)(body["prompt"].as_str().unwrap(), body["n_probs"].as_u64().unwrap());
            let top: Vec<Value> = next
                .iter()
                .map(|(token, logprob)| {
                    let id = token.chars().next().unwrap() as i64;
                    json!({ "id": id, "token": token, "bytes": [], "logprob": logprob })
                })
                .collect();
            let first = json!({ "id": 65, "token": "A", "top_logprobs": top });
            return MockResponse::json(&json!({ "content": "A", "completion_probabilities": [first] }));
        }
        MockResponse::Status(404, "not found".into())
    })
    .await
}

static SERVER_COUNT: AtomicUsize = AtomicUsize::new(0);

/// A fresh server root per model: label tokens are cached per server root and model.
fn model(server: &MockServer) -> Model {
    let count = SERVER_COUNT.fetch_add(1, Ordering::Relaxed);
    let base_url = format!("{}/llama-{count}/v1", server.base_url);
    let mut model = classifier_model("llama-cpp-classify", "llama.cpp", "qwen", &base_url);
    model.context_window = 32_768;
    model
}

fn options() -> ClassifierOptions {
    ClassifierOptions {
        request: RequestOptions { max_retries: Some(0), ..RequestOptions::default() },
        temperature: None,
    }
}

fn answer_by_prompt(prompt: &str, _depth: u64) -> Vec<(&'static str, f64)> {
    if prompt.contains("Answer Yes or No.") {
        return vec![("Y", -0.05), ("N", -3.0)];
    }
    if prompt.contains("Answer with one level number.") {
        return vec![("2", -0.2), ("1", -1.8), ("0", -4.0)];
    }
    vec![("B", -0.3), ("A", -1.5), ("C", -3.0)]
}

fn sample_context() -> ClassifierContext {
    context(json!({
        "state": { "message": "Help! My payouts have been failing for 3 days." },
        "questions": {
            "team": {
                "type": "choice",
                "instructions": "Which team should handle this?",
                "criteria": { "billing": "Payments and refunds", "technical": "Bugs and outages", "sales": "" },
            },
            "urgent": {
                "type": "bool",
                "instructions": "Does this convey urgency?",
                "criteria": { "true": "The user needs help soon", "false": "No time pressure" },
            },
            "severity": { "type": "score", "instructions": "How severe is this?", "criteria": ["low", "medium", "high"] },
        },
    }))
}

fn pick_context() -> ClassifierContext {
    context(json!({
        "state": {},
        "questions": { "pick": { "type": "choice", "instructions": "Pick one", "criteria": { "a": "", "b": "" } } },
    }))
}

fn requests_to<'a>(requests: &'a [RecordedRequest], endpoint: &str) -> Vec<&'a RecordedRequest> {
    requests.iter().filter(|request| request.path.ends_with(endpoint)).collect()
}

fn completion_depths(server: &MockServer) -> Vec<u64> {
    requests_to(&server.requests(), "/completion")
        .iter()
        .map(|request| request.body["n_probs"].as_u64().unwrap())
        .collect()
}

async fn classify(model: &Model, context: &ClassifierContext, options: ClassifierOptions) -> ClassifierResult {
    LlamaCppClassify.classify(model, context, options).await
}

#[tokio::test]
async fn answers_choice_bool_and_score_questions_from_label_log_probabilities() {
    let server = fake_server(Fake { next: Arc::new(answer_by_prompt), tokenize: word_tokens, ..Fake::default() }).await;
    let model = model(&server);
    let mut options = options();
    options.request.api_key = Some("local".into());
    let result = classify(&model, &sample_context(), options).await;

    assert_eq!(result.error_message, None);
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    let choice = label_probabilities(&[-1.5, -0.3, -3.0], 1.0);
    let answers = answers_json(&result);
    let team = json!({
        "type": "choice",
        "choice": "technical",
        "probabilities": { "billing": choice[0], "technical": choice[1], "sales": choice[2] },
        "confidence": peak_confidence(&choice),
    });
    assert_eq!(answers["team"], team);
    let urgent = label_probabilities(&[-0.05, -3.0], 1.0)[0];
    assert_eq!(answers["urgent"], json!({ "type": "bool", "probability": urgent }));
    let levels = label_probabilities(&[-4.0, -1.8, -0.2], 1.0);
    let severity =
        json!({ "type": "score", "score": levels[1] + 2.0 * levels[2], "confidence": peak_confidence(&levels) });
    assert_eq!(answers["severity"], severity);

    let root = llama_server_root(&model.base_url).replacen(&server.base_url, "", 1);
    let requests = server.requests();
    for request in &requests {
        assert!(request.path.starts_with(&format!("{root}/")), "{}", request.path);
        assert_eq!(request.body["model"], "qwen");
        assert_eq!(request.headers["authorization"], "Bearer local");
    }
    let completion = &requests_to(&requests, "/completion")[0].body;
    assert_eq!((completion["n_predict"].as_u64(), completion["n_probs"].as_u64()), (Some(1), Some(256)));
    assert_eq!(
        (completion["post_sampling_probs"].as_bool(), completion["cache_prompt"].as_bool()),
        (Some(false), Some(true))
    );
    let template = &requests_to(&requests, "/apply-template")[0].body;
    assert_eq!(template["chat_template_kwargs"], json!({ "enable_thinking": false }));
}

#[test]
fn repeats_the_state_around_all_questions_and_ends_with_this_questions_labels() {
    let rendered = render_question(&sample_context(), "team").unwrap();
    let state = "State:\n{\n \"message\": \"Help! My payouts have been failing for 3 days.\"\n}";
    assert_eq!(rendered.labels, ["A", "B", "C"]);
    assert_eq!(rendered.keys, ["billing", "technical", "sales"]);
    let expected = [
        state,
        "",
        "Task: answer each of the following questions about the state.",
        "",
        "Question: Which team should handle this?",
        "",
        "Options:",
        "- billing: Payments and refunds",
        "- technical: Bugs and outages",
        "- sales",
        "",
        "Question: Does this convey urgency?",
        "",
        "Yes means: The user needs help soon",
        "No means: No time pressure",
        "",
        "Question: How severe is this?",
        "",
        "Levels:",
        "0. low",
        "1. medium",
        "2. high",
        "",
        state,
        "",
        "Question: Which team should handle this?",
        "",
        "Options:",
        "A. billing: Payments and refunds",
        "B. technical: Bugs and outages",
        "C. sales",
        "",
        "Answer with one letter.",
    ];
    assert_eq!(rendered.content, expected.join("\n"));
}

#[test]
fn shares_everything_before_the_final_question_across_the_questions_of_a_request() {
    let context = sample_context();
    let content = |id: &str| render_question(&context, id).unwrap().content;
    let prefix = |id: &str| {
        let content = content(id);
        content[..content.rfind("Question:").unwrap()].to_string()
    };
    assert_eq!(prefix("urgent"), prefix("team"));
    assert_eq!(prefix("severity"), prefix("team"));
    assert!(content("urgent").ends_with("No means: No time pressure\n\nAnswer Yes or No."));
    assert!(content("severity").ends_with("2. high\n\nAnswer with one level number."));
}

#[tokio::test]
async fn divides_label_log_probabilities_by_the_temperature() {
    let server = fake_server(Fake::default()).await;
    let result =
        classify(&model(&server), &pick_context(), ClassifierOptions { temperature: Some(2.0), ..options() }).await;
    let expected = label_probabilities(&[-0.1 / 2.0, -2.5 / 2.0], 1.0);
    assert_eq!(answers_json(&result)["pick"]["probabilities"], json!({ "a": expected[0], "b": expected[1] }));
    assert_eq!(label_probabilities(&[-0.1, -2.5], 2.0), expected);
}

#[tokio::test]
async fn rejects_non_positive_temperatures_before_sending_requests() {
    let server = fake_server(Fake::default()).await;
    let result =
        classify(&model(&server), &sample_context(), ClassifierOptions { temperature: Some(0.0), ..options() }).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert!(result.error_message.unwrap().contains("Temperature must be a positive number, got 0"));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn retries_deeper_readouts_when_a_label_is_missing_and_fails_without_inventing_zeros() {
    let next: NextFn =
        Arc::new(|_, depth| if depth < 4096 { vec![("A", -0.1)] } else { vec![("A", -0.1), ("B", -9.0)] });
    let deep = fake_server(Fake { next, ..Fake::default() }).await;
    let recovered = classify(&model(&deep), &pick_context(), options()).await;
    assert_eq!(recovered.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(completion_depths(&deep), [256, 4096]);

    let never = fake_server(Fake { next: Arc::new(|_, _| vec![("A", -0.1)]), ..Fake::default() }).await;
    let failed = classify(&model(&never), &pick_context(), options()).await;
    assert_eq!(failed.stop_reason, ClassifierStopReason::Error);
    assert!(failed.answers.is_empty());
    let message = failed.error_message.unwrap();
    assert!(message.contains("did not rank labels B for pick within the top 32768 tokens"), "{message}");
    assert_eq!(completion_depths(&never), [256, 4096, 32768]);
}

#[tokio::test]
async fn closes_a_reasoning_block_the_template_leaves_open() {
    let server = fake_server(Fake { template: Some(|_| "<|assistant|>\n<think>".into()), ..Fake::default() }).await;
    classify(&model(&server), &pick_context(), options()).await;
    let requests = server.requests();
    assert_eq!(requests_to(&requests, "/completion")[0].body["prompt"], "<|assistant|>\n<think></think>");
}

#[tokio::test]
async fn reads_labels_in_reply_position_and_rejects_labels_that_are_not_one_token() {
    // A tokenizer that merges a newline with a following letter falls back to the label alone.
    let merging = |content: &str| match content.starts_with('\n') && content.len() > 1 {
        true => vec![1000],
        false => char_tokens(content),
    };
    let server = fake_server(Fake { tokenize: merging, ..Fake::default() }).await;
    let merged = classify(&model(&server), &pick_context(), options()).await;
    assert_eq!(merged.stop_reason, ClassifierStopReason::Stop);

    // The default fake tokenizer splits "Yes" into three tokens.
    let split = fake_server(Fake::default()).await;
    let context = context(json!({
        "state": {},
        "questions": { "ok": { "type": "bool", "instructions": "OK?", "criteria": { "true": "", "false": "" } } },
    }));
    let result = classify(&model(&split), &context, options()).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert!(result.error_message.unwrap().contains("Label \"Yes\" is not a single token for qwen"));
}

#[tokio::test]
async fn caches_label_tokens_per_server_and_model() {
    let server = fake_server(Fake::default()).await;
    let model = model(&server);
    classify(&model, &pick_context(), options()).await;
    let first = requests_to(&server.requests(), "/tokenize").len();
    classify(&model, &pick_context(), options()).await;
    assert!(first > 0);
    assert_eq!(requests_to(&server.requests(), "/tokenize").len(), first);
}

#[tokio::test]
async fn validates_option_counts_before_sending_requests() {
    let server = fake_server(Fake::default()).await;
    let criteria: serde_json::Map<String, Value> = (0..63).map(|index| (format!("option{index}"), json!(""))).collect();
    let too_many = context(json!({
        "state": {},
        "questions": { "pick": { "type": "choice", "instructions": "Pick", "criteria": criteria } },
    }));
    let too_few = context(json!({
        "state": {},
        "questions": { "rate": { "type": "score", "instructions": "Rate", "criteria": ["only"] } },
    }));
    let too_many = classify(&model(&server), &too_many, options()).await;
    let too_few = classify(&model(&server), &too_few, options()).await;
    assert!(too_many.error_message.unwrap().contains("A choice question needs 2 to 62 options, got 63"));
    assert!(too_few.error_message.unwrap().contains("A score question needs 2 to 10 levels, got 1"));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn passes_completion_payloads_and_responses_through_the_request_hooks() {
    let server = fake_server(Fake::default()).await;
    let payloads: Arc<Mutex<Vec<Value>>> = Arc::default();
    let statuses: Arc<Mutex<Vec<u16>>> = Arc::default();
    let (seen_payloads, seen_statuses) = (payloads.clone(), statuses.clone());
    let mut options = options();
    options.request.on_payload = Some(crate::registry::payload_hook(move |payload: &Value, _: &Model| {
        seen_payloads.lock().unwrap().push(payload.clone());
        let mut payload = payload.clone();
        payload["id_slot"] = json!(1);
        Some(payload)
    }));
    options.request.on_response = Some(Arc::new(move |response: &ProviderResponse, _: &Model| {
        seen_statuses.lock().unwrap().push(response.status);
    }));
    classify(&model(&server), &pick_context(), options).await;

    let payloads = payloads.lock().unwrap();
    assert_eq!((payloads.len(), payloads[0]["n_predict"].as_u64()), (1, Some(1)));
    assert_eq!(*statuses.lock().unwrap(), [200]);
    assert_eq!(requests_to(&server.requests(), "/completion")[0].body["id_slot"], 1);
}

#[tokio::test]
async fn reports_server_errors_and_cancellation() {
    let body = r#"{"error":{"message":"context overflow"}}"#;
    let failing = MockServer::routed(move |_| MockResponse::Status(400, body.into())).await;
    let failed = classify(&model(&failing), &sample_context(), options()).await;
    assert_eq!(failed.stop_reason, ClassifierStopReason::Error);
    let message = failed.error_message.unwrap();
    assert!(message.contains("llama.cpp error (400)") && message.contains("context overflow"), "{message}");

    let server = fake_server(Fake::default()).await;
    let options = options();
    options.request.cancel.cancel();
    let aborted = classify(&model(&server), &sample_context(), options).await;
    assert_eq!(aborted.stop_reason, ClassifierStopReason::Aborted);
}

#[tokio::test]
async fn rejects_models_for_other_classifier_apis() {
    let server = fake_server(Fake::default()).await;
    let mut other = model(&server);
    other.api = "typesafe-system-one".into();
    let result = classify(&other, &sample_context(), options()).await;
    assert!(result.error_message.unwrap().contains("Unsupported classifier API: typesafe-system-one"));
    assert!(server.requests().is_empty());
}

#[test]
fn derives_the_server_root_from_openai_compatible_base_urls() {
    assert_eq!(llama_server_root("http://127.0.0.1:8080/v1/"), "http://127.0.0.1:8080");
    assert_eq!(llama_server_root("https://example.com/prefix/v1"), "https://example.com/prefix");
    assert_eq!(llama_server_root("http://127.0.0.1:8080"), "http://127.0.0.1:8080");
}

#[test]
fn computes_typesafe_confidence_and_expected_scores() {
    assert!((peak_confidence(&[0.89, 0.06, 0.05]) - 0.835).abs() < 1e-9);
    assert_eq!(peak_confidence(&[0.5, 0.5]), 0.0);
    assert_eq!(peak_confidence(&[1.0, 0.0, 0.0]), 1.0);
    let question =
        ClassifierQuestion::Score { instructions: String::new(), criteria: vec!["a".into(), "b".into(), "c".into()] };
    let keys = ["0", "1", "2"].map(String::from);
    let answer = answer_from_probabilities(&question, &keys, &[0.2, 0.3, 0.5]);
    assert_eq!(answer, ClassifierAnswer::Score { score: 1.3, confidence: peak_confidence(&[0.2, 0.3, 0.5]) });
}
