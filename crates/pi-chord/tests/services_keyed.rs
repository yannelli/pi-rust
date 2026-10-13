//! Port of upstream `test/services.test.ts`: keyed observation through a binding.

mod support;

use std::sync::{Arc, Mutex};

use pi_chord::services::{
    RemoteServiceBindingOptions, RemoteServiceProvider, RemoteServices, ServiceDefinition, ServiceHandle, ServiceImpl,
    create_loopback_service_transport, create_remote_service_binding, define_service, observe_handler,
};
use pi_chord::{BACKGROUND_CONTEXT, ChordError, Context, Json, json, replicated_state};
use support::{Recorder, wait_for};

struct QuestionDialogs;

#[derive(Clone)]
struct Observed {
    question: Option<Json>,
    service: ServiceHandle,
    context: Context,
}

#[tokio::test]
async fn hydrates_keyed_state_before_observe_handlers_and_fences_reused_keys() {
    let dialogs = define_service::<QuestionDialogs>("test.question-dialog").unwrap();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::keyed(&dialogs)]).unwrap();
    assert_eq!(
        pi_chord::services::catalogue_to_json(provider.catalogue()),
        json!([
            {"serviceId": "test.question-dialog", "mode": "keyed"}
        ])
    );
    let errors = Recorder::new();
    let sink = errors.clone();
    let namespace = create_remote_service_binding(RemoteServiceBindingOptions {
        on_error: Some(Arc::new(move |error: ChordError| sink.push(error.to_string()))),
        ..RemoteServiceBindingOptions::new(vec![dialogs.erase()], create_loopback_service_transport(&provider))
    })
    .unwrap();
    let observed: Recorder<Observed> = Recorder::new();
    let record = observed.clone();
    let stop = namespace
        .observe(
            &dialogs,
            observe_handler(move |service, context| {
                let question = service.state("request").and_then(|state| state.value()).unwrap();
                record.push(Observed { question, service, context });
                async { Ok(()) }
            }),
        )
        .unwrap();
    namespace.ready(&BACKGROUND_CONTEXT).await.unwrap();
    assert!(errors.get().is_empty());

    let first_request = replicated_state(json!({"question": "First?"}));
    let calls: Arc<Mutex<Vec<(Json, bool)>>> = Arc::default();
    let call_sink = calls.clone();
    let close_first = provider
        .spawn(
            &dialogs,
            "invocation-1",
            ServiceImpl::new().state("request", &first_request).method("submit", move |args, context| {
                call_sink.lock().unwrap().push((args[0].clone(), context.abort_signal().is_none()));
                async { Ok(Some(json!({"accepted": true}))) }
            }),
        )
        .unwrap();
    wait_for(|| observed.len() == 1).await;
    assert_eq!(observed.get()[0].question, Some(json!({"question": "First?"})));

    let first = observed.get()[0].clone();
    first_request.change(&BACKGROUND_CONTEXT, |draft| draft.set("question", "Updated?")).unwrap();
    let request = first.service.state("request").unwrap();
    assert_eq!(request.value().unwrap(), Some(json!({"question": "Updated?"})));
    let reply = first.service.call("submit", vec![json!("yes")], &BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(reply, Some(json!({"accepted": true})));
    assert_eq!(calls.lock().unwrap().clone(), [(json!("yes"), true)]);

    close_first.unsubscribe();
    assert!(first.context.is_cancelled());
    assert!(request.value().unwrap_err().to_string().contains("observation is closed"));
    let late = first.service.call("submit", vec![json!("late")], &BACKGROUND_CONTEXT).await;
    assert!(late.unwrap_err().to_string().contains("observation is closed"));

    let second_request = replicated_state(json!({"question": "Again?"}));
    let close_second = provider
        .spawn(
            &dialogs,
            "invocation-1",
            ServiceImpl::new()
                .state("request", &second_request)
                .method("submit", |_, _| async { Ok(Some(json!({"accepted": false}))) }),
        )
        .unwrap();
    wait_for(|| observed.len() == 2).await;
    let second = observed.get()[1].clone();
    assert_eq!(second.question, Some(json!({"question": "Again?"})));
    assert!(errors.get().is_empty());

    stop.unsubscribe();
    assert!(second.context.is_cancelled());
    let request = second.service.state("request").and_then(|state| state.value());
    assert!(request.unwrap_err().to_string().contains("observation is closed"));
    let late = second.service.call("submit", vec![json!("late")], &BACKGROUND_CONTEXT).await;
    assert!(late.unwrap_err().to_string().contains("observation is closed"));
    close_second.unsubscribe();
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    provider.dispose().unwrap();
}
