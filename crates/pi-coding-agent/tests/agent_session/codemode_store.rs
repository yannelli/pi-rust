//! The codemode store and `models` in a session (upstream `agent-session-codemode.test.ts`
//! "codemode options and store" and "codemode models"): `codemode-store` entries per branch, and
//! classifier and image calls with the catalog's credentials.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use pi_ai::classify::{ClassifierAnswer, ClassifierApi, ClassifierContext, ClassifierOptions, ClassifierResult};
use pi_ai::images::{AssistantImages, ImagesApi, ImagesContext, ImagesOptions, ImagesStopReason};
use pi_ai::{ContentBlock, Model};
use pi_coding_agent::agent_session::NavigateTreeOptions;
use pi_coding_agent::session::SessionManager;
use serde_json::{Value, json};

use crate::codemode::{check_saved_images, codemode_harness, image_block, output, run, usage};
use crate::harness::{Harness, Options, open_session_with};
use crate::mcp_fake::TINY_PNG;
use crate::tool_support::{declared_description, result_text};

const INCREMENT: &str = "const next = (load(\"count\") ?? 0) + 1;\nstore(\"count\", next);\nreturn next;";

fn store_entries(h: &Harness) -> Vec<Value> {
    let manager = h.session.session_manager();
    let branch = manager.get_branch(None);
    let custom = branch.iter().map(|entry| Value::Object(entry.raw().clone()));
    custom.filter(|entry| entry["customType"] == "codemode-store").map(|entry| entry["data"].clone()).collect()
}

#[tokio::test]
async fn persists_store_writes_as_custom_entries_for_later_calls() {
    let h = codemode_harness(&["codemode"], Vec::new(), Options::default()).await;
    assert_eq!(output(&run(&h, INCREMENT).await), "1");
    assert_eq!(output(&run(&h, INCREMENT).await), "2");
    assert_eq!(
        store_entries(&h),
        [json!({ "set": { "count": 1 }, "delete": [] }), json!({ "set": { "count": 2 }, "delete": [] })]
    );
    let appended = h.events_of("entry_appended");
    assert_eq!(appended.iter().filter(|event| event["entry"]["customType"] == "codemode-store").count(), 2);

    let removed = run(&h, "store(\"count\", undefined);\nreturn load(\"count\") === undefined;").await;
    assert_eq!(output(&removed), "true");
    assert_eq!(store_entries(&h).last().unwrap(), &json!({ "set": {}, "delete": ["count"] }));
}

#[tokio::test]
async fn appends_nothing_for_failed_scripts_or_scripts_without_writes() {
    let h = codemode_harness(&["codemode"], Vec::new(), Options::default()).await;
    assert!(run(&h, "store(\"count\", 5);\nthrow new Error(\"boom\");").await.is_error);
    assert!(!run(&h, "return load(\"count\") ?? \"missing\";").await.is_error);
    assert!(store_entries(&h).is_empty());
}

#[tokio::test]
async fn loads_the_values_written_on_the_current_branch() {
    let h = codemode_harness(&["codemode"], Vec::new(), Options::default()).await;
    run(&h, INCREMENT).await;
    let first_prompt = {
        let manager = h.session.session_manager();
        let branch = manager.get_branch(None);
        branch.iter().find(|entry| entry.raw()["type"] == "message").unwrap().id().to_string()
    };
    assert_eq!(output(&run(&h, INCREMENT).await), "2");

    // Branch from the first prompt: the store entries written after it are on another path.
    h.session.session_manager().branch(&first_prompt).unwrap();
    assert_eq!(output(&run(&h, INCREMENT).await), "1");

    // Navigating back to the second write on the first path brings its value back.
    let second_write = {
        let manager = h.session.session_manager();
        let writes =
            manager.entries().iter().filter(|entry| entry.raw().get("customType") == Some(&json!("codemode-store")));
        writes.map(|entry| entry.id().to_string()).nth(1).unwrap()
    };
    h.session.navigate_tree(&second_write, NavigateTreeOptions::default()).await.unwrap();
    assert_eq!(output(&run(&h, INCREMENT).await), "3");
}

#[tokio::test]
async fn keeps_the_store_when_the_session_is_resumed() {
    let h = codemode_harness(&["codemode"], Vec::new(), Options::default()).await;
    run(&h, INCREMENT).await;
    run(&h, INCREMENT).await;
    let manager = SessionManager::open(&h.session.session_file().unwrap()).unwrap();
    let resumed = open_session_with(&h.services, manager, |_| {}).await;
    h.provider.push(pi_ai::providers::faux::FauxResponse::tool_call("r1", "codemode", json!({ "code": INCREMENT })));
    h.provider.push(pi_ai::providers::faux::FauxResponse::text("done"));
    resumed.prompt("go", Vec::new()).await.unwrap();
    let result = crate::tool_support::tool_result(&resumed, "codemode");
    assert_eq!(output(&result), "3");
}

static NEXT_API: AtomicU64 = AtomicU64::new(1);

/// Base URL, API key, and input of one call.
type Seen<T> = Mutex<Vec<(String, Option<String>, T)>>;

#[derive(Default)]
struct Observed {
    classify: Seen<Value>,
    images: Seen<Vec<ContentBlock>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

struct FakeModels(Arc<Observed>);

impl ClassifierApi for FakeModels {
    fn classify(
        &self,
        model: &Model,
        context: &ClassifierContext,
        options: ClassifierOptions,
    ) -> BoxFuture<'static, ClassifierResult> {
        let (observed, model) = (self.0.clone(), model.clone());
        let text = context.state.get("text").cloned().unwrap_or(Value::Null);
        Box::pin(async move {
            let active = observed.active.fetch_add(1, Ordering::SeqCst) + 1;
            observed.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(10)).await;
            observed.active.fetch_sub(1, Ordering::SeqCst);
            observed.classify.lock().unwrap().push((model.base_url.clone(), options.request.api_key, text.clone()));
            let mut result = ClassifierResult::new(&model);
            if text == "explode" {
                result.stop_reason = pi_ai::classify::ClassifierStopReason::Error;
                result.error_message = Some("classifier exploded".into());
                return result;
            }
            let probability = if text == "good" { 0.9 } else { 0.1 };
            result.answers.insert("approved", ClassifierAnswer::Bool { probability });
            result.usage = Some(usage(300, 0.001));
            result
        })
    }
}

impl ImagesApi for FakeModels {
    fn generate_images(
        &self,
        model: &Model,
        context: &ImagesContext,
        options: ImagesOptions,
    ) -> BoxFuture<'static, AssistantImages> {
        let (observed, model, input) = (self.0.clone(), model.clone(), context.input.clone());
        Box::pin(async move {
            observed.images.lock().unwrap().push((model.base_url.clone(), options.request.api_key, input.clone()));
            let prompt = input.iter().find_map(ContentBlock::as_text).unwrap_or_default().to_string();
            let mut result = AssistantImages::new(&model);
            if prompt == "explode" {
                result.stop_reason = ImagesStopReason::Error;
                result.error_message = Some("painter exploded".into());
                return result;
            }
            result.output = vec![ContentBlock::text(format!("painted {prompt}")), image_block()];
            result.usage = Some(usage(100, 0.04));
            result
        })
    }
}

/// A codemode session with a `scorer` provider of a classifier `judge` and an image `painter`.
async fn models_harness() -> (Harness, Arc<Observed>) {
    let n = NEXT_API.fetch_add(1, Ordering::Relaxed);
    let (classifier, images) = (format!("test-classifier-{n}"), format!("test-images-{n}"));
    let observed = Arc::new(Observed::default());
    pi_ai::register_classifier_api(classifier.clone(), Arc::new(FakeModels(observed.clone())));
    pi_ai::register_images_api(images.clone(), Arc::new(FakeModels(observed.clone())));
    let providers = json!({ "scorer": { "baseUrl": "https://classifier.test/v1", "api": classifier, "apiKey": "secret-key",
        "models": [
            { "id": "judge", "name": "Judge", "type": "classifier", "api": classifier, "input": ["text"],
              "contextWindow": 1000, "headers": { "X-Secret": "hunter2" } },
            { "id": "painter", "name": "Painter", "type": "image", "api": images, "baseUrl": "https://images.test/v1",
              "input": ["text", "image"], "output": ["text", "image"] }
        ] } });
    let h = codemode_harness(&["codemode"], Vec::new(), Options { providers, ..Options::default() }).await;
    (h, observed)
}

const QUESTIONS: &str =
    r#"{ approved: { type: "bool", instructions: "Approval?", criteria: { true: "yes", false: "no" } } }"#;

#[tokio::test]
async fn declares_models_and_writes_the_docs_they_point_to() {
    let h = codemode_harness(&["codemode"], Vec::new(), Options::default()).await;
    let description = declared_description(&h.session, "codemode").unwrap();
    assert!(description.contains("`models`: classifiers and image generation"));
    let docs = h.agent_dir.join("docs").join("codemode.md");
    assert!(description.contains(&docs.display().to_string()), "{description}");
    let text = std::fs::read_to_string(docs).unwrap();
    assert!(text.contains("models.getModelOfType"));
}

#[tokio::test]
async fn lists_models_and_classifies_with_catalog_auth_ignoring_script_fields() {
    let (h, observed) = models_harness().await;
    let code = format!(
        r#"
        const [model] = await models.getAvailableOfType("classifier", "scorer");
        const listed = await models.getModelsOfType("classifier");
        const same = await models.getModelOfType("classifier", "scorer", "judge");
        const texts = ["good", "bad", "good", "bad", "good", "bad"];
        const results = await Promise.all(
            texts.map((text) => models.classify({{ ...model, baseUrl: "https://evil.test" }}, {{ state: {{ text }}, questions: {QUESTIONS} }})),
        );
        return {{
            id: model.id,
            headers: "headers" in model,
            listed: listed.some((entry) => entry.provider === "scorer" && entry.id === "judge"),
            same: same.id,
            missing: (await models.getModelOfType("classifier", "scorer", "nope")) === undefined,
            probabilities: results.map((r) => r.answers.approved.probability),
            cost: results[0].usage.cost.total,
        }};
        "#
    );
    let result = run(&h, &code).await;
    assert!(!result.is_error, "{}", result_text(&result));
    let value: Value = serde_json::from_str(&output(&result)).unwrap();
    assert_eq!(
        value,
        json!({ "id": "judge", "headers": false, "listed": true, "same": "judge", "missing": true,
            "probabilities": [0.9, 0.1, 0.9, 0.1, 0.9, 0.1], "cost": 0.001 })
    );
    let seen = observed.classify.lock().unwrap().clone();
    assert_eq!(seen.len(), 6);
    let catalog = Some("secret-key".to_string());
    assert!(seen.iter().all(|(url, key, _)| url == "https://classifier.test/v1" && *key == catalog));
    // Six classifications with at most four in flight.
    assert_eq!(observed.max_active.load(Ordering::SeqCst), 4);
    let calls = result.details.as_ref().unwrap()["calls"].as_array().unwrap().clone();
    let rows: Vec<Value> =
        calls.iter().map(|call| json!([call["name"], call["args"], call["status"], call["cost"]])).collect();
    assert_eq!(rows, vec![json!(["models.classify", "scorer/judge", "ok", 0.001]); 6]);
    // The classifications' usage becomes the codemode result's usage.
    let usage = result.usage.as_ref().unwrap();
    assert_eq!(usage.input, 1800);
    assert!((usage.cost.total - 0.006).abs() < 1e-10);
    assert!((h.session.session_stats().stats.cost - 0.006).abs() < 1e-10);
}

#[tokio::test]
async fn generates_images_with_catalog_auth_and_attaches_them_through_image() {
    let (h, observed) = models_harness().await;
    let code = format!(
        r#"
        const [model] = await models.getAvailableOfType("image", "scorer");
        const reference = {{ type: "image", data: "{TINY_PNG}", mimeType: "image/png" }};
        const generated = await models.generateImages({{ ...model, baseUrl: "https://evil.test" }}, {{ input: [{{ type: "text", text: "a fox" }}, reference] }});
        for (const block of generated.output) {{
            if (block.type === "image") image(block);
            else text(block.text);
        }}
        const failed = await models.generateImages(model, {{ input: [{{ type: "text", text: "explode" }}] }});
        const attempt = async (fn) => {{ try {{ await fn(); return "ok"; }} catch (error) {{ return error.message; }} }};
        return {{
            id: model.id,
            stopReason: generated.stopReason,
            failed: [failed.stopReason, failed.errorMessage],
            wrongType: await attempt(() => models.generateImages({{ provider: "scorer", id: "judge" }}, {{ input: [] }})),
        }};
        "#
    );
    let result = run(&h, &code).await;
    assert!(!result.is_error, "{}", result_text(&result));
    let text = check_saved_images(&output(&result));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[..5], ["==> text 1/2 <==", "painted a fox", "<saved>", "<image>", "==> text 2/2 <=="]);
    let value: Value = serde_json::from_str(&lines[5..].join("\n")).unwrap();
    assert_eq!(
        value,
        json!({ "id": "painter", "stopReason": "stop", "failed": ["error", "painter exploded"],
            "wrongType": "\"scorer/judge\" is a classifier model, not an image model. List the image models you can use with models.getAvailableOfType(\"image\")." })
    );
    assert_eq!(result.content[2], image_block());
    let seen = observed.images.lock().unwrap().clone();
    let auth: Vec<(&str, Option<&str>)> = seen.iter().map(|(url, key, _)| (url.as_str(), key.as_deref())).collect();
    assert_eq!(auth, [("https://images.test/v1", Some("secret-key")); 2]);
    assert_eq!(seen[0].2, [ContentBlock::text("a fox"), image_block()]);
    assert!((result.usage.as_ref().unwrap().cost.total - 0.04).abs() < 1e-10);
}

#[tokio::test]
async fn reports_provider_errors_as_results_and_unknown_models_as_exceptions() {
    let (h, _) = models_harness().await;
    let code = format!(
        r#"
        const model = await models.getModelOfType("classifier", "scorer", "judge");
        const failed = await models.classify(model, {{ state: {{ text: "explode" }}, questions: {QUESTIONS} }});
        const textOnly = await models.classify(model, {{
            state: {{ text: "good" }},
            images: [{{ type: "image", data: "aW1hZ2U=", mimeType: "image/png" }}],
            questions: {QUESTIONS},
        }});
        const attempt = async (fn) => {{ try {{ await fn(); return "ok"; }} catch (error) {{ return error.message; }} }};
        return {{
            failed: [failed.stopReason, failed.errorMessage],
            textOnly: [textOnly.stopReason, textOnly.errorMessage],
            unknown: await attempt(() => models.classify({{ provider: "scorer", id: "nope" }}, {{}})),
        }};
        "#
    );
    let result = run(&h, &code).await;
    assert!(!result.is_error, "{}", result_text(&result));
    let value: Value = serde_json::from_str(&output(&result)).unwrap();
    assert_eq!(value["failed"], json!(["error", "classifier exploded"]));
    assert_eq!(value["textOnly"], json!(["error", "Model scorer/judge does not accept image input"]));
    assert_eq!(
        value["unknown"],
        "Unknown classifier model \"scorer/nope\". List the classifier models you can use with models.getAvailableOfType(\"classifier\")."
    );
}
