//! Port of upstream `test/codemode-models.test.ts`: `models.*` in durable code mode, with a classifier and an image
//! model of a test provider.

use async_trait::async_trait;
use pi_codemode::{CodemodeModels, ModelType};
use pi_durable::codemode::create_codemode_extension;
use pi_durable::env::SharedEnv;
use pi_durable::harness::usage::USAGE_DOC;
use tokio_util::sync::CancellationToken;

use super::env::HookedEnv;
use super::results::tool_result;
use super::*;

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

#[derive(Default)]
struct Calls {
    classify: AtomicUsize,
    images: AtomicUsize,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

/// Holds classifier calls until `open` resolves; with `at_four`, it opens a little after four run at once.
struct Gate {
    open: Mutex<Deferred>,
    at_four: AtomicBool,
}

struct TestModels {
    calls: Arc<Calls>,
    gate: Arc<Gate>,
}

fn catalog() -> Vec<Value> {
    let cost = json!({ "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 });
    vec![
        json!({
            "id": "judge", "name": "Judge", "api": "test-classifier", "provider": "test",
            "baseUrl": "https://example.invalid", "input": ["text"], "cost": cost,
            "headers": { "authorization": "secret" }, "type": "classifier", "contextWindow": 1000,
        }),
        json!({
            "id": "painter", "name": "Painter", "api": "test-images", "provider": "test",
            "baseUrl": "https://example.invalid", "input": ["text"], "cost": cost, "type": "image", "output": ["image"],
        }),
    ]
}

fn of_type(model_type: ModelType, provider: Option<&str>) -> Vec<Value> {
    let matches = |model: &Value| {
        model["type"] == model_type.as_str() && provider.is_none_or(|provider| model["provider"] == provider)
    };
    catalog().into_iter().filter(matches).collect()
}

fn result_of(model: &Value, fields: Value) -> Value {
    let mut result =
        json!({ "api": model["api"], "provider": model["provider"], "model": model["id"], "timestamp": 0 });
    result.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
    result
}

#[async_trait]
impl CodemodeModels for TestModels {
    fn get_models_of_type(&self, model_type: ModelType, provider: Option<&str>) -> Vec<Value> {
        of_type(model_type, provider)
    }
    async fn get_available_of_type(
        &self,
        model_type: ModelType,
        provider: Option<&str>,
        _: CancellationToken,
    ) -> anyhow::Result<Vec<Value>> {
        Ok(of_type(model_type, provider))
    }
    fn get_model_of_type(&self, model_type: ModelType, provider: &str, id: &str) -> Option<Value> {
        of_type(model_type, Some(provider)).into_iter().find(|model| model["id"] == id)
    }
    async fn classify(&self, model: &Value, context: Value, _: CancellationToken) -> anyhow::Result<Value> {
        self.calls.classify.fetch_add(1, Ordering::SeqCst);
        if context["state"]["fail"] == true {
            let fields = json!({ "answers": {}, "stopReason": "error", "errorMessage": "quota exceeded" });
            return Ok(result_of(model, fields));
        }
        let in_flight = self.calls.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.calls.max_in_flight.fetch_max(in_flight, Ordering::SeqCst);
        let open = self.gate.open.lock().unwrap().clone();
        if self.gate.at_four.load(Ordering::SeqCst) && in_flight == 4 {
            let open = open.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                open.resolve();
            });
        }
        open.wait().await;
        self.calls.in_flight.fetch_sub(1, Ordering::SeqCst);
        let usage = json!({
            "input": 10, "output": 2, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 12,
            "cost": { "input": 0.001, "output": 0.0002, "cacheRead": 0, "cacheWrite": 0, "total": 0.0012 },
        });
        let fields = json!({ "answers": { "good": { "value": true } }, "usage": usage, "stopReason": "stop" });
        Ok(result_of(model, fields))
    }
    async fn generate_images(&self, model: &Value, _: Value, _: CancellationToken) -> anyhow::Result<Value> {
        self.calls.images.fetch_add(1, Ordering::SeqCst);
        let output = json!([{ "type": "image", "data": PNG, "mimeType": "image/png" }]);
        Ok(result_of(model, json!({ "output": output, "stopReason": "stop" })))
    }
}

struct ModelsChat {
    setup: ChatSetup,
    calls: Arc<Calls>,
    gate: Arc<Gate>,
}

/// A chat whose code mode extension gives scripts the test provider's models.
fn models_chat() -> ModelsChat {
    let setup = chat_setup(Default::default());
    let (calls, gate) = (Arc::new(Calls::default()), Arc::new(Gate { open: Mutex::default(), at_four: false.into() }));
    gate.open.lock().unwrap().resolve();
    let mut options = CodemodeToolOptions::new(new_sandbox());
    options.models = Some(Arc::new(TestModels { calls: calls.clone(), gate: gate.clone() }));
    setup.registry.install(create_codemode_extension(options)).unwrap();
    ModelsChat { setup, calls, gate }
}

struct Ran {
    text: String,
    details: CodemodeToolDetails,
    content: Vec<ContentBlock>,
    usage: Option<Json>,
}

async fn run(chat: &ModelsChat, code: &str, env: Option<SharedEnv>) -> Ran {
    chat.setup.faux.set_responses([script(code), done()]);
    let options = match env {
        Some(env) => chat.setup.options().with_shared_env(env),
        None => chat.setup.options(),
    };
    let (harness, root) = open_chat_with(Arc::new(MemoryStorage::new()), &chat.setup, options).await;
    let id = submit(&root, "go").await;
    wait(&harness, id).await;
    let (text, result) = tool_result(&root).await;
    let usage = harness.snapshot(&USAGE_DOC, root.id, &ctx()).await.unwrap();
    harness.close(&ctx()).await.unwrap();
    let details = serde_json::from_value(result.details.clone().unwrap()).unwrap();
    Ran { text: without_header(&text), details, content: result.content, usage }
}

fn rows(details: &CodemodeToolDetails) -> Vec<(&str, CodemodeNestedCallStatus)> {
    details.calls.iter().map(|call| (call.name.as_str(), call.status)).collect()
}

#[tokio::test]
async fn reads_the_model_catalog_without_credentials_in_headers() {
    let chat = models_chat();
    let code = r#"const classifiers = await models.getModelsOfType("classifier", "test");
        const one = await models.getModelOfType("classifier", "test", "judge");
        const available = await models.getAvailableOfType("image");
        return [classifiers.map((m) => m.id), one.id, "headers" in one, available.map((m) => m.id)];"#;
    let ran = run(&chat, code, None).await;
    assert_eq!(serde_json::from_str::<Value>(&ran.text).unwrap(), json!([["judge"], "judge", false, ["painter"]]));
}

#[tokio::test]
async fn classifies_through_an_internal_tool_a_nested_call_with_its_usage_hidden_from_tools_and_listings() {
    let chat = models_chat();
    let code = r#"const model = await models.getModelOfType("classifier", "test", "judge");
        const result = await models.classify(model, {
            state: { text: "hi" },
            questions: { good: { type: "bool", instructions: "Is it good?", criteria: { true: "yes", false: "no" } } },
        });
        const found = (await searchTools("classify generate images models")).length;
        const described = (await describeTool("pi.codemode.models.classify")) !== undefined;
        return [result.stopReason, result.answers.good.value, Object.keys(tools).some((name) => name.includes("models")), ALL_TOOLS.some((tool) => tool.name.includes("models")), found, described];"#;
    let ran = run(&chat, code, None).await;
    assert_eq!(serde_json::from_str::<Value>(&ran.text).unwrap(), json!(["stop", true, false, false, 0, false]));
    assert_eq!(chat.calls.classify.load(Ordering::SeqCst), 1);
    assert_eq!(rows(&ran.details), [("models.classify", CodemodeNestedCallStatus::Ok)]);
    // Counted once, under the internal tool, by the Harness.
    let usage = ran.usage.unwrap();
    let total = usage["tools"]["pi.codemode.models.classify"]["cost"]["total"].as_f64().unwrap();
    assert!((total - 0.0012).abs() < 1e-9, "{total}");
}

#[tokio::test]
async fn resolves_a_model_call_that_failed_with_its_result_as_the_coding_agent_does() {
    let chat = models_chat();
    let code = r#"const model = await models.getModelOfType("classifier", "test", "judge");
        const result = await models.classify(model, {
            state: { fail: true },
            questions: { good: { type: "bool", instructions: "?", criteria: { true: "y", false: "n" } } },
        });
        return [result.stopReason, result.errorMessage];"#;
    let ran = run(&chat, code, None).await;
    assert_eq!(serde_json::from_str::<Value>(&ran.text).unwrap(), json!(["error", "quota exceeded"]));
    // The row still shows the failure.
    assert_eq!(rows(&ran.details), [("models.classify", CodemodeNestedCallStatus::Error)]);
}

#[tokio::test]
async fn rejects_a_bad_classifier_context_with_the_expected_shape_before_any_request() {
    let chat = models_chat();
    let code = r#"const model = await models.getModelOfType("classifier", "test", "judge");
        try { await models.classify(model, { prompt: "x" }); } catch (error) { return error.message; }"#;
    let ran = run(&chat, code, None).await;
    assert!(ran.text.contains("context.state must be an object"), "{}", ran.text);
    assert!(ran.text.contains("Expected context:"), "{}", ran.text);
    assert_eq!(chat.calls.classify.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn runs_at_most_four_model_calls_at_once() {
    let chat = models_chat();
    *chat.gate.open.lock().unwrap() = Deferred::default();
    chat.gate.at_four.store(true, Ordering::SeqCst);
    let code = r#"const model = await models.getModelOfType("classifier", "test", "judge");
        const context = { state: {}, questions: { good: { type: "bool", instructions: "?", criteria: { true: "y", false: "n" } } } };
        return (await Promise.all(Array.from({ length: 8 }, () => models.classify(model, context)))).length;"#;
    let ran = run(&chat, code, None).await;
    assert_eq!(ran.text, "8");
    assert_eq!(chat.calls.classify.load(Ordering::SeqCst), 8);
    assert_eq!(chat.calls.max_in_flight.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn does_not_start_a_model_call_that_waited_for_a_slot_after_the_script_ended() {
    let chat = models_chat();
    *chat.gate.open.lock().unwrap() = Deferred::default();
    let directory = tempfile::tempdir().unwrap();
    // The result spills to a file after the script ended and before the tool returns. Then the running calls finish
    // and free their slots, which the two waiting calls would take.
    let mut env = HookedEnv::new(directory.path());
    let gate = chat.gate.clone();
    env.before_temp_dir = Some(Arc::new(move || {
        gate.open.lock().unwrap().resolve();
        Box::pin(tokio::time::sleep(Duration::from_millis(100)))
    }));
    let code = r#"const model = await models.getModelOfType("classifier", "test", "judge");
        const context = { state: {}, questions: { good: { type: "bool", instructions: "?", criteria: { true: "y", false: "n" } } } };
        for (let i = 0; i < 6; i++) models.classify(model, context).catch(() => {});
        // A journaled call after them: by its result, the first four have been launched.
        await models.getModelsOfType("image");
        return "x".repeat(60000);"#;
    let ran = run(&chat, code, Some(Arc::new(env))).await;
    assert!(ran.text.contains("[Full output: "), "{}", ran.text);
    assert_eq!(chat.calls.classify.load(Ordering::SeqCst), 4);
    if let Some(path) = ran.details.full_output_path {
        super::results::remove_parent(&path);
    }
}

#[tokio::test]
async fn notes_generated_images_the_script_did_not_show() {
    let code = r#"const model = await models.getModelOfType("image", "test", "painter");
        const result = await models.generateImages(model, { input: [{ type: "text", text: "a cat" }] });
        return result.output.length;"#;
    let hidden = run(&models_chat(), code, None).await;
    let note = "Note: models.generateImages() returned 1 image that the script did not show";
    assert!(hidden.text.contains(note), "{}", hidden.text);
    let code = r#"const model = await models.getModelOfType("image", "test", "painter");
        const result = await models.generateImages(model, { input: [{ type: "text", text: "a cat" }] });
        image(result.output[0]);"#;
    let shown = run(&models_chat(), code, None).await;
    assert!(!shown.text.contains("Note:"), "{}", shown.text);
    assert!(shown.content.iter().any(|item| matches!(item, ContentBlock::Image { .. })));
}
