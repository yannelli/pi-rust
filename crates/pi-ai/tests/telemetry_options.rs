//! Port of upstream `test/telemetry-options.test.ts`: the explicit telemetry parent span
//! survives stream, deferred, and image dispatch unchanged.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use pi_ai::{
    ApiProvider, AssistantImages, AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Context,
    DeferredHandle, DeferredOptions, ImagesApi, ImagesContext, ImagesOptions, Model, RequestOptions, StopReason,
    StreamOptions, event_channel,
};
use pi_telemetry::{TelemetrySpan, noop_telemetry_span};

type Observed = Arc<Mutex<Vec<Option<Arc<dyn TelemetrySpan>>>>>;

fn model(api: &str, model_type: Option<&str>) -> Model {
    Model {
        id: "model".into(),
        name: "Model".into(),
        api: api.into(),
        provider: "telemetry-provider".into(),
        base_url: "https://example.test".into(),
        model_type: model_type.map(String::from),
        ..Model::default()
    }
}

fn completed(model: &Model) -> AssistantMessageEventStream {
    let (sender, stream) = event_channel();
    sender.push(AssistantMessageEvent::Done { reason: StopReason::Stop, message: AssistantMessage::empty(model) });
    stream
}

struct Recorder(Observed);

impl ApiProvider for Recorder {
    fn stream(&self, model: &Model, _context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        self.0.lock().unwrap().push(options.telemetry);
        completed(model)
    }

    fn fetch_deferred(
        &self,
        model: &Model,
        _: &DeferredHandle,
        options: DeferredOptions,
    ) -> AssistantMessageEventStream {
        self.0.lock().unwrap().push(options.request.telemetry);
        completed(model)
    }

    fn cancel_deferred(
        &self,
        _: &Model,
        _: &DeferredHandle,
        options: DeferredOptions,
    ) -> BoxFuture<'static, Result<(), String>> {
        self.0.lock().unwrap().push(options.request.telemetry);
        Box::pin(async { Ok(()) })
    }
}

impl ImagesApi for Recorder {
    fn generate_images(
        &self,
        model: &Model,
        _: &ImagesContext,
        options: ImagesOptions,
    ) -> BoxFuture<'static, AssistantImages> {
        self.0.lock().unwrap().push(options.request.telemetry);
        let result = AssistantImages {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            ..AssistantImages::default()
        };
        Box::pin(async move { result })
    }
}

fn all_match(observed: &Observed, expected: &Arc<dyn TelemetrySpan>, count: usize) {
    let observed = observed.lock().unwrap();
    assert_eq!(observed.len(), count);
    assert!(observed.iter().all(|value| value.as_ref().is_some_and(|value| Arc::ptr_eq(value, expected))));
}

#[tokio::test]
async fn survives_stream_and_deferred_dispatch() {
    let observed: Observed = Arc::default();
    pi_ai::register_api_provider("telemetry-test", Arc::new(Recorder(observed.clone())));
    let model = model("telemetry-test", None);
    let telemetry = noop_telemetry_span();
    let options =
        StreamOptions { api_key: Some("key".into()), telemetry: Some(telemetry.clone()), ..StreamOptions::default() };
    pi_ai::complete(&model, &Context::default(), options).await;
    let request =
        RequestOptions { api_key: Some("key".into()), telemetry: Some(telemetry.clone()), ..RequestOptions::default() };
    let deferred = DeferredOptions { request, wait_ms: None };
    let handle = DeferredHandle {
        provider: model.provider.clone(),
        model_id: model.id.clone(),
        api: model.api.clone(),
        id: "response".into(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    };
    pi_ai::fetch_deferred(&model, &handle, deferred.clone()).await;
    pi_ai::cancel_deferred(&model, &handle, deferred).await.unwrap();
    all_match(&observed, &telemetry, 3);
}

#[tokio::test]
async fn survives_image_dispatch() {
    let observed: Observed = Arc::default();
    pi_ai::register_images_api("telemetry-test-images", Arc::new(Recorder(observed.clone())));
    let model = model("telemetry-test-images", Some("image"));
    let telemetry = noop_telemetry_span();
    let request =
        RequestOptions { api_key: Some("key".into()), telemetry: Some(telemetry.clone()), ..RequestOptions::default() };
    let context = ImagesContext { input: vec![pi_ai::ContentBlock::text("circle")] };
    pi_ai::generate_images(&model, &context, ImagesOptions { request, metadata: None }).await;
    all_match(&observed, &telemetry, 1);
}
