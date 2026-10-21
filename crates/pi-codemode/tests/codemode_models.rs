//! Ported from upstream `agent-session-codemode.test.ts` ("codemode models"): `models.*` from
//! scripts against a model registry stand-in.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::ContentBlock;
use pi_codemode::{CodemodeModels, CodemodeNestedCallStatus, CodemodeTool, CodemodeToolOptions, ModelType};
use serde_json::{Value, json};
use support::*;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Observations {
    observed: Mutex<Vec<(String, Value)>>,
    image_requests: Mutex<Vec<(String, Value)>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

/// The registry stand-in. Clones share what the test observes.
#[derive(Clone, Default)]
struct FakeModels(Arc<Observations>);

impl std::ops::Deref for FakeModels {
    type Target = Observations;
    fn deref(&self) -> &Observations {
        &self.0
    }
}

fn catalog() -> Vec<Value> {
    vec![
        json!({
            "type": "classifier", "id": "judge", "name": "Judge", "api": "test-classifier", "provider": "scorer",
            "baseUrl": "https://classifier.test/v1", "input": ["text"], "contextWindow": 1000,
            "headers": { "X-Secret": "hunter2" },
        }),
        json!({
            "type": "image", "id": "painter", "name": "Painter", "api": "test-images", "provider": "scorer",
            "baseUrl": "https://images.test/v1", "input": ["text", "image"], "output": ["text", "image"],
        }),
    ]
}

fn usage(input: u64, cost: f64) -> Value {
    json!({ "input": input, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": input,
            "cost": { "input": cost, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": cost } })
}

#[async_trait]
impl CodemodeModels for FakeModels {
    fn get_models_of_type(&self, model_type: ModelType, provider: Option<&str>) -> Vec<Value> {
        catalog()
            .into_iter()
            .filter(|model| model["type"] == model_type.as_str() && provider.is_none_or(|p| model["provider"] == p))
            .collect()
    }

    async fn get_available_of_type(
        &self,
        model_type: ModelType,
        provider: Option<&str>,
        _: CancellationToken,
    ) -> anyhow::Result<Vec<Value>> {
        Ok(self.get_models_of_type(model_type, provider))
    }

    fn get_model_of_type(&self, model_type: ModelType, provider: &str, id: &str) -> Option<Value> {
        self.get_models_of_type(model_type, Some(provider)).into_iter().find(|model| model["id"] == id)
    }

    async fn classify(&self, model: &Value, context: Value, _: CancellationToken) -> anyhow::Result<Value> {
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(10)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        let text = context["state"]["text"].clone();
        self.observed.lock().unwrap().push((model["baseUrl"].as_str().unwrap().into(), text.clone()));
        let base = json!({ "api": model["api"], "provider": model["provider"], "model": model["id"], "timestamp": 0 });
        let mut result = base.as_object().cloned().unwrap();
        let has_images = context["images"].as_array().is_some_and(|images| !images.is_empty());
        if text == "explode" || has_images {
            let message =
                if has_images { "Model scorer/judge does not accept image input" } else { "classifier exploded" };
            result.extend([
                ("answers".into(), json!({})),
                ("stopReason".into(), json!("error")),
                ("errorMessage".into(), json!(message)),
            ]);
            return Ok(Value::Object(result));
        }
        let probability = if text == "good" { 0.9 } else { 0.1 };
        result.extend([
            ("answers".into(), json!({ "approved": { "type": "bool", "probability": probability } })),
            ("usage".into(), usage(300, 0.001)),
            ("stopReason".into(), json!("stop")),
        ]);
        Ok(Value::Object(result))
    }

    async fn generate_images(&self, model: &Value, context: Value, _: CancellationToken) -> anyhow::Result<Value> {
        self.image_requests.lock().unwrap().push((model["baseUrl"].as_str().unwrap().into(), context["input"].clone()));
        let prompt = context["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "text")
            .map(|block| block["text"].clone());
        let base = json!({ "api": model["api"], "provider": model["provider"], "model": model["id"], "timestamp": 0 });
        let mut result = base.as_object().cloned().unwrap();
        if prompt == Some(json!("explode")) {
            result.extend([
                ("output".into(), json!([])),
                ("stopReason".into(), json!("error")),
                ("errorMessage".into(), json!("painter exploded")),
            ]);
            return Ok(Value::Object(result));
        }
        result.extend([
            ("output".into(), json!([
                { "type": "text", "text": format!("painted {}", prompt.and_then(|p| p.as_str().map(str::to_string)).unwrap_or_default()) },
                { "type": "image", "data": TINY_PNG_BASE64, "mimeType": "image/png" },
            ])),
            ("usage".into(), usage(100, 0.04)),
            ("stopReason".into(), json!("stop")),
        ]);
        Ok(Value::Object(result))
    }
}

fn setup() -> (Arc<FakeHost>, FakeModels) {
    let models = FakeModels::default();
    (Arc::new(FakeHost::default().with_models(Box::new(models.clone()))), models)
}

const QUESTIONS: &str =
    r#"{ approved: { type: "bool", instructions: "Approval?", criteria: { true: "yes", false: "no" } } }"#;

#[test]
fn declares_models_only_with_a_model_registry() {
    let (host, _) = setup();
    let tool = CodemodeTool::new(
        host,
        CodemodeToolOptions { docs_path: "/docs/codemode.md".into(), ..CodemodeToolOptions::default() },
    );
    let description = pi_agent::AgentTool::description(&tool);
    assert!(
        description.contains("`models`: classifiers and image generation") && description.contains("/docs/codemode.md")
    );
    let plain = CodemodeTool::new(Arc::new(FakeHost::default()), CodemodeToolOptions::default());
    assert!(!pi_agent::AgentTool::description(&plain).contains("`models`"));
}

#[tokio::test]
async fn lists_models_and_classifies_with_catalog_entries() {
    let (host, models) = setup();
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
    let output = run_script(&host, &code).await;
    assert!(!output.is_error, "{}", result_text(&output));
    let value: Value = serde_json::from_str(&result_text(&output)).unwrap();
    assert_eq!(
        value,
        json!({ "id": "judge", "headers": false, "listed": true, "same": "judge", "missing": true,
                "probabilities": [0.9, 0.1, 0.9, 0.1, 0.9, 0.1], "cost": 0.001 })
    );
    let observed = models.observed.lock().unwrap();
    assert_eq!(observed.len(), 6);
    assert!(observed.iter().all(|(base_url, _)| base_url == "https://classifier.test/v1"));
    // Six classifications with at most four in flight.
    assert_eq!(models.max_active.load(Ordering::SeqCst), 4);
    let rows: Vec<_> = output
        .details
        .calls
        .iter()
        .map(|call| (call.name.as_str(), call.args.as_str(), call.status, call.cost))
        .collect();
    assert_eq!(rows, vec![("models.classify", "scorer/judge", CodemodeNestedCallStatus::Ok, Some(0.001)); 6]);
    assert!(output.details.calls.iter().all(|call| call.id.starts_with("call-1/models.classify/")));
    let usage = output.usage.unwrap();
    assert_eq!(usage.input, 1800);
    assert!((usage.cost.total - 0.006).abs() < 1e-10);
}

#[tokio::test]
async fn generates_images_with_catalog_entries_and_attaches_them_through_image() {
    let (host, models) = setup();
    let code = format!(
        r#"
        const [model] = await models.getAvailableOfType("image", "scorer");
        const reference = {{ type: "image", data: "{TINY_PNG_BASE64}", mimeType: "image/png" }};
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
    let output = run_script(&host, &code).await;
    assert!(!output.is_error);
    let text = check_saved_images(&result_text(&output));
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(lines[..5], ["==> text 1/2 <==", "painted a fox", "<saved>", "<image>", "==> text 2/2 <=="]);
    let value: Value = serde_json::from_str(&lines[5..].join("\n")).unwrap();
    assert_eq!(
        value,
        json!({
            "id": "painter", "stopReason": "stop", "failed": ["error", "painter exploded"],
            "wrongType": "\"scorer/judge\" is a classifier model, not an image model. List the image models you can use with models.getAvailableOfType(\"image\").",
        })
    );
    assert_eq!(output.content[2], ContentBlock::Image { data: TINY_PNG_BASE64.into(), mime_type: "image/png".into() });
    let requests = models.image_requests.lock().unwrap();
    assert!(requests.iter().all(|(base_url, _)| base_url == "https://images.test/v1"));
    assert_eq!(
        requests[0].1,
        json!([{ "type": "text", "text": "a fox" }, { "type": "image", "data": TINY_PNG_BASE64, "mimeType": "image/png" }])
    );
    let rows: Vec<_> = output
        .details
        .calls
        .iter()
        .map(|call| (call.args.as_str(), call.status, call.cost, call.error.as_deref()))
        .collect();
    assert_eq!(
        rows,
        [
            ("scorer/painter", CodemodeNestedCallStatus::Ok, Some(0.04), None),
            ("scorer/painter", CodemodeNestedCallStatus::Error, None, Some("painter exploded")),
        ]
    );
    assert!((output.usage.unwrap().cost.total - 0.04).abs() < 1e-10);
}

#[tokio::test]
async fn notes_generated_images_the_script_did_not_show() {
    let (host, _) = setup();
    let code = "const [model] = await models.getAvailableOfType(\"image\", \"scorer\");\nconst generated = await models.generateImages(model, { input: [{ type: \"text\", text: \"a fox\" }] });\nreturn generated.stopReason;";
    assert_eq!(
        result_text(&run_script(&host, code).await),
        "stop\nNote: models.generateImages() returned 1 image that the script did not show. Show each image block of result.output with image(block)."
    );
}

#[tokio::test]
async fn reports_provider_errors_as_results_and_invalid_arguments_as_exceptions() {
    let (host, _) = setup();
    let code = format!(
        r#"
        const model = await models.getModelOfType("classifier", "scorer", "judge");
        const failed = await models.classify(model, {{ state: {{ text: "explode" }}, questions: {QUESTIONS} }});
        const textOnly = await models.classify(model, {{ state: {{ text: "good" }}, images: [{{ type: "image", data: "aW1hZ2U=", mimeType: "image/png" }}], questions: {QUESTIONS} }});
        const attempt = async (fn) => {{ try {{ await fn(); return "ok"; }} catch (error) {{ return error.message; }} }};
        return {{
            failed: [failed.stopReason, failed.errorMessage],
            textOnly: [textOnly.stopReason, textOnly.errorMessage],
            badClassifierImage: await attempt(() => models.classify(model, {{ state: {{}}, images: [{{ data: "aW1hZ2U=" }}], questions: {QUESTIONS} }})),
            badType: await attempt(() => models.getModelsOfType("video")),
            unknown: await attempt(() => models.classify({{ provider: "scorer", id: "nope" }}, {{}})),
            noModel: await attempt(() => models.classify("judge", {{}})),
            undefinedModel: await attempt(() => models.classify(undefined, {{}})),
            noState: await attempt(() => models.classify(model, {{ questions: {QUESTIONS} }})),
            badQuestion: await attempt(() => models.classify(model, {{ state: {{}}, questions: {{ kind: {{ type: "choice", instructions: "Kind?", criteria: ["a", "b"] }} }} }})),
            badImage: await attempt(() => models.generateImages({{ provider: "scorer", id: "painter" }}, {{ prompt: "a fox" }})),
            arrayModel: await attempt(() => models.classify(["scorer", "judge"], {{}})),
            arrayContext: await attempt(() => models.classify(model, [])),
            arrayState: await attempt(() => models.classify(model, {{ state: [], questions: {QUESTIONS} }})),
            arrayQuestions: await attempt(() => models.classify(model, {{ state: {{}}, questions: [] }})),
            arrayQuestion: await attempt(() => models.classify(model, {{ state: {{}}, questions: {{ kind: [] }} }})),
            noInstructions: await attempt(() => models.classify(model, {{ state: {{}}, questions: {{ kind: {{ type: "bool", criteria: {{ true: "y", false: "n" }} }} }} }})),
            badBool: await attempt(() => models.classify(model, {{ state: {{}}, questions: {{ kind: {{ type: "bool", instructions: "Ok?", criteria: ["y", "n"] }} }} }})),
            badKind: await attempt(() => models.classify(model, {{ state: {{}}, questions: {{ kind: {{ type: "rank", instructions: "Rank?" }} }} }})),
            arrayImagesContext: await attempt(() => models.generateImages({{ provider: "scorer", id: "painter" }}, [])),
            badImageBlock: await attempt(() => models.generateImages({{ provider: "scorer", id: "painter" }}, {{ input: [{{ type: "text", text: "a fox" }}, {{ type: "image", data: "aW1hZ2U=" }}] }})),
            badSplit: await attempt(() => models.getModelOfType("classifier", "scorer/judge")),
        }};
        "#
    );
    let output = run_script(&host, &code).await;
    assert!(!output.is_error);
    let value: Value = serde_json::from_str(&result_text(&output)).unwrap();
    let text = |key: &str| value[key].as_str().unwrap_or_default().to_string();
    assert_eq!(value["failed"], json!(["error", "classifier exploded"]));
    assert_eq!(value["textOnly"], json!(["error", "Model scorer/judge does not accept image input"]));
    assert!(
        text("badClassifierImage")
            .contains("models.classify() context.images[0] must be an image block, got { data }.")
    );
    assert!(text("badType").contains("Unknown model type \"video\""));
    assert_eq!(
        text("unknown"),
        "Unknown classifier model \"scorer/nope\". List the classifier models you can use with models.getAvailableOfType(\"classifier\")."
    );
    assert!(
        text("noModel").contains("models.classify() expects a classifier model as its first argument, got a string.")
    );
    assert!(
        text("undefinedModel").contains("models.getModelOfType() returns undefined for an unknown provider or id.")
    );
    assert!(text("noState").contains("models.classify() context.state must be an object, got undefined."));
    assert!(text("noState").contains("codemode.md"));
    assert!(
        text("badQuestion").contains(
            "context.questions.kind is a \"choice\" question, so criteria must map each label to its meaning."
        )
    );
    assert!(
        text("badImage")
            .contains("models.generateImages() context.input must be a non-empty array of blocks, got undefined.")
    );
    assert!(text("badSplit").contains("The provider and the id are separate arguments"));
    assert!(
        text("arrayModel")
            .contains("models.classify() expects a classifier model as its first argument, got an array.")
    );
    assert!(
        text("arrayContext")
            .contains("models.classify() expects a context object as its second argument, got an empty array.")
    );
    assert!(text("arrayState").contains("models.classify() context.state must be an object, got an empty array."));
    assert!(
        text("arrayQuestions")
            .contains("models.classify() context.questions must map question IDs to questions, got an empty array.")
    );
    assert!(
        text("arrayQuestion")
            .contains("models.classify() context.questions.kind must be a question object, got an empty array.")
    );
    assert!(text("noInstructions").contains("models.classify() context.questions.kind.instructions must be a string."));
    assert!(text("badBool").contains(
        "context.questions.kind is a \"bool\" question, so criteria must be { true: string, false: string }."
    ));
    assert!(
        text("badKind")
            .contains("context.questions.kind.type must be \"choice\", \"score\", or \"bool\", got \"rank\".")
    );
    assert!(
        text("arrayImagesContext")
            .contains("models.generateImages() expects a context object as its second argument, got an empty array.")
    );
    assert!(
        text("badImageBlock")
            .contains("models.generateImages() context.input[1] must be a text or image block, got { type, data }.")
    );
    let rows: Vec<_> =
        output.details.calls.iter().map(|call| (call.name.as_str(), call.status, call.error.as_deref())).collect();
    assert_eq!(
        rows,
        [
            ("models.classify", CodemodeNestedCallStatus::Error, Some("classifier exploded")),
            (
                "models.classify",
                CodemodeNestedCallStatus::Error,
                Some("Model scorer/judge does not accept image input")
            ),
        ]
    );
    assert_eq!(output.usage, None);
}
