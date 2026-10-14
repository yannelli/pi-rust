//! Port of upstream `test/experimental-client-tui.test.ts`: the client TUI over loopback service sources.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::ThinkingLevel;
use pi_chord::node::{FacetBundleArtifact, FacetBundleEntry, FacetBundlePlugin, integrity_of};
use pi_chord::services::{
    ObserveHandler, RemoteServiceBinding, RemoteServiceBindingOptions, RemoteServiceProvider, RemoteServiceSource,
    RemoteServiceSourceOpenOptions, RemoteServices, ServiceCatalogueEntry, ServiceDefinition, ServiceHandle,
    ServiceImpl, ServiceRef, create_loopback_service_transport, create_remote_service_binding,
};
use pi_chord::state::MutableReplicatedState;
use pi_chord::{BACKGROUND_CONTEXT, ChordResult, Context, Json, Unsubscribe};
use pi_durable::harness::types::{AgentChange, ModelRef as DurableModelRef};
use pi_durable::harness::{CreateOptions, Harness, HarnessOptions, RegistryReader, create_registry};
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message, faux_provider,
};
use pi_tui::env::MapEnv;
use pi_tui::testing::VirtualTerminal;
use pi_tui::{Tui, TuiAltScreenOptions, TuiOptions};
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::cli::ClientCommand;
use super::client::resolve_path;
use super::client_tui::{ClientTuiOptions, ExperimentalClientTui};
use super::client_tui_parts::ClientTuiServer;
use super::plugins::bundled::create_presentation_facet_data;
use super::services::agent_controller::{AGENT_CONTROLLER, agent_controller_impl, create_agent_controller};
use super::services::models::{MODELS, ModelRef, Models, models_impl};
use super::services::plugins::{
    PRESENTATION_PLUGINS, PrepareSessionRequest, PresentationPlugins, SESSION_PLUGINS, SessionPlugins,
    presentation_plugins_impl, session_plugins_impl,
};
use super::services::sessions::{
    SESSION_DIRECTORY, SESSION_MANAGEMENT, SessionCreateOptions, SessionManagement, SessionSummary,
    session_management_impl,
};
use super::services::transcript::TRANSCRIPT;
use crate::interactive_components::test_support::setup;

const SERVER_ID: &str = "00000000-0000-4000-8000-000000000001";
const RELOAD_SOURCE: &str = "\"use strict\";\nconst { defineFacet, defineService } = require(\"@earendil-works/chord\");\nconst Models = defineService(\"pi.models\");\nmodule.exports = { __esModule: true, default: defineFacet({ id: \"test-tui-facet\", setup(env) { env.use(Models); } }) };\n";

fn json(value: Value) -> Json {
    Json::from(&value)
}

fn session(session_id: &str, created_at: f64) -> Value {
    json!({ "serverId": SERVER_ID, "sessionId": session_id, "createdAt": created_at })
}

fn publish(state: &MutableReplicatedState, value: Value) {
    state.replace(&BACKGROUND_CONTEXT, json(value)).unwrap();
}

/// A loopback source over `binding`; server sources bind on `ready`, Session sources on attachment.
struct LoopbackSource {
    provider: RemoteServiceProvider,
    binding: RemoteServiceBinding,
    session: bool,
}

struct LoopbackServices(RemoteServiceBinding, bool);

#[async_trait]
impl RemoteServices for LoopbackServices {
    fn use_service(&self, service: &ServiceRef) -> ChordResult<ServiceHandle> {
        self.0.use_service(service)
    }
    fn observe(&self, service: &ServiceRef, handler: ObserveHandler) -> ChordResult<Unsubscribe> {
        self.0.observe(service, handler)
    }
    async fn ready(&self, context: &Context) -> ChordResult<()> {
        if self.1 {
            self.0.rebind(true, context).await?;
        }
        self.0.ready(context).await
    }
    async fn dispose(&self, _context: &Context) -> ChordResult<()> {
        Ok(())
    }
}

#[async_trait]
impl RemoteServiceSource for LoopbackSource {
    fn accepts_unavailable_services(&self) -> bool {
        self.session
    }
    async fn catalogue(&self, _context: &Context) -> ChordResult<Vec<ServiceCatalogueEntry>> {
        Ok(if self.session { Vec::new() } else { self.provider.catalogue().to_vec() })
    }
    fn open(&self, _options: RemoteServiceSourceOpenOptions) -> ChordResult<Arc<dyn RemoteServices>> {
        Ok(Arc::new(LoopbackServices(self.binding.clone(), !self.session)))
    }
}

fn loopback(
    services: &[&ServiceRef],
    implementations: Vec<(&ServiceRef, ServiceImpl)>,
    session: bool,
) -> LoopbackSource {
    let definitions: Vec<ServiceDefinition> =
        services.iter().map(|service| ServiceDefinition::singleton(service)).collect();
    let provider = RemoteServiceProvider::new(&definitions).unwrap();
    for (service, implementation) in implementations {
        provider.provide(service, implementation).unwrap();
    }
    let binding = create_remote_service_binding(RemoteServiceBindingOptions {
        bound: false,
        ..RemoteServiceBindingOptions::new(
            services.iter().map(|service| (*service).clone()).collect(),
            create_loopback_service_transport(&provider),
        )
    })
    .unwrap();
    LoopbackSource { provider, binding, session }
}

#[derive(Default)]
struct Calls {
    creates: Mutex<usize>,
    prepared: Mutex<Vec<PrepareSessionRequest>>,
    presentation_reloads: Mutex<usize>,
    session_reloads: Mutex<usize>,
    selected: Mutex<Vec<ModelRef>>,
    thinking: Mutex<Vec<ThinkingLevel>>,
}

struct Mock {
    calls: Arc<Calls>,
    directory: MutableReplicatedState,
    attachment: MutableReplicatedState,
    models: MutableReplicatedState,
    reload_data: Json,
}

#[async_trait]
impl SessionManagement for Mock {
    async fn create(&self, _options: SessionCreateOptions, _context: &Context) -> ChordResult<SessionSummary> {
        *self.calls.creates.lock().unwrap() += 1;
        publish(&self.directory, json!({ "revision": 2, "sessions": [session("one", 1.0), session("two", 2.0)] }));
        Ok(SessionSummary { server_id: SERVER_ID.into(), session_id: "two".into(), created_at: 2.0 })
    }
    async fn remove(&self, _session_id: String, _context: &Context) -> ChordResult<()> {
        Ok(())
    }
    async fn attach(&self, session_id: String, _context: &Context) -> ChordResult<()> {
        publish(&self.attachment, json!({ "status": "attaching", "sessionId": session_id }));
        Ok(())
    }
    async fn detach(&self, _context: &Context) -> ChordResult<()> {
        publish(&self.attachment, json!({ "status": "detached" }));
        Ok(())
    }
}

#[async_trait]
impl PresentationPlugins for Mock {
    async fn prepare_session(&self, request: PrepareSessionRequest, _context: &Context) -> ChordResult<Json> {
        self.calls.prepared.lock().unwrap().push(request);
        Ok(self.reload_data.clone())
    }
    async fn reload(&self, _context: &Context) -> ChordResult<Json> {
        *self.calls.presentation_reloads.lock().unwrap() += 1;
        Ok(self.reload_data.clone())
    }
}

#[async_trait]
impl SessionPlugins for Mock {
    async fn reload(&self, _context: &Context) -> ChordResult<()> {
        *self.calls.session_reloads.lock().unwrap() += 1;
        Ok(())
    }
}

#[async_trait]
impl Models for Mock {
    async fn cycle_thinking(&self, _context: &Context) -> ChordResult<()> {
        Ok(())
    }
    async fn get_thinking_levels(&self, _context: &Context) -> ChordResult<Vec<ThinkingLevel>> {
        Ok(vec![ThinkingLevel::Off, ThinkingLevel::High])
    }
    async fn refresh(&self, _context: &Context) -> ChordResult<()> {
        Ok(())
    }
    async fn select(&self, model: ModelRef, _context: &Context) -> ChordResult<()> {
        self.calls.selected.lock().unwrap().push(model.clone());
        let mut value = self.models.value().to_serde();
        value["configuration"]["model"] = serde_json::to_value(&model).unwrap();
        publish(&self.models, value);
        Ok(())
    }
    async fn select_thinking(&self, level: ThinkingLevel, _context: &Context) -> ChordResult<()> {
        self.calls.thinking.lock().unwrap().push(level);
        let mut value = self.models.value().to_serde();
        value["configuration"]["thinkingLevel"] = serde_json::to_value(level).unwrap();
        publish(&self.models, value);
        Ok(())
    }
}

fn reload_data() -> Json {
    let artifact = FacetBundleArtifact {
        plugin: FacetBundlePlugin { id: "test-tui-plugin".into(), version: None },
        entry_name: "tui".into(),
        entry: FacetBundleEntry {
            file: "tui.cjs".into(),
            integrity: integrity_of(RELOAD_SOURCE.as_bytes()),
            external_imports: vec!["@earendil-works/chord".into()],
            source_map: None,
        },
        source: RELOAD_SOURCE.into(),
        source_map_contents: None,
    };
    create_presentation_facet_data(&[artifact])
}

async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

fn screen(ui: &ExperimentalClientTui) -> String {
    ui.render(80).join("\n")
}

/// Upstream `openFauxConversation` with one answer that waits for `finish`; it records user prompts.
async fn open_faux_conversation(
    prompts: Arc<Mutex<Vec<String>>>,
    finish: Arc<Notify>,
) -> (Harness, pi_durable::harness::Conversation) {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses([FauxResponseStep::async_factory(move |context, _, _, _| {
        if let Some(pi_ai::Message::User(user)) = context.messages.last() {
            prompts.lock().unwrap().push(super::durable::tui_parts::user_text(&user.content));
        }
        let finish = finish.clone();
        Box::pin(async move {
            finish.notified().await;
            Ok(faux_assistant_message("remote answer", FauxMessageOptions::default()))
        })
    })]);
    let registry: Arc<dyn RegistryReader> = Arc::new(create_registry());
    let options = HarnessOptions::new(faux.provider(), registry);
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &BACKGROUND_CONTEXT).await.unwrap();
    let model = faux.get_model();
    let agent = AgentChange::model(DurableModelRef { provider: model.provider.clone(), model_id: model.id.clone() });
    let conversation = harness.root(&BACKGROUND_CONTEXT, CreateOptions::default().with_agent(agent)).await.unwrap();
    (harness, conversation)
}

async fn run_case(kind: &str, command: ClientCommand, session_id: &str, creates: usize) {
    let calls = Arc::new(Calls::default());
    let mock = Arc::new(Mock {
        calls: calls.clone(),
        directory: MutableReplicatedState::new(json(json!({ "revision": 1, "sessions": [session("one", 1.0)] }))),
        attachment: MutableReplicatedState::new(json(json!({ "status": "detached" }))),
        models: MutableReplicatedState::new(json(json!({
            "catalog": { "revision": 1, "availableModels": [
                { "provider": "test", "modelId": "one", "name": "Model One", "reasoning": false },
                { "provider": "test", "modelId": "two", "name": "Model Two", "reasoning": true } ] },
            "configuration": { "model": { "provider": "test", "modelId": "one" }, "thinkingLevel": "off" },
            "refresh": { "status": "idle" } }))),
        reload_data: reload_data(),
    });
    let connection = MutableReplicatedState::new(json(json!({ "status": "connected", "since": "now" })));
    let (prompts, finish_prompt) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Notify::new()));
    let (harness, conversation) = open_faux_conversation(prompts.clone(), finish_prompt.clone()).await;
    let transcript_state = conversation.view_state(&BACKGROUND_CONTEXT).await.unwrap();
    let server = loopback(
        &[&SESSION_DIRECTORY, &SESSION_MANAGEMENT, &PRESENTATION_PLUGINS],
        vec![
            (&SESSION_DIRECTORY, ServiceImpl::new().state("state", &mock.directory)),
            (&SESSION_MANAGEMENT, session_management_impl(mock.clone())),
            (&PRESENTATION_PLUGINS, presentation_plugins_impl(mock.clone())),
        ],
        false,
    );
    let session_source = loopback(
        &[&MODELS, &AGENT_CONTROLLER, &SESSION_PLUGINS, &TRANSCRIPT],
        vec![
            (&SESSION_PLUGINS, session_plugins_impl(mock.clone())),
            (&MODELS, models_impl(mock.clone(), &mock.models)),
            (&AGENT_CONTROLLER, agent_controller_impl(create_agent_controller(harness.clone(), conversation.clone()))),
            (&TRANSCRIPT, ServiceImpl::new().state("state", &transcript_state)),
        ],
        true,
    );
    let (session_binding, attachment) = (session_source.binding.clone(), mock.attachment.clone());
    let server_binding = server.binding.clone();
    let tui_server = ClientTuiServer {
        server_id: SERVER_ID.into(),
        radius: true,
        server: Arc::new(server),
        session: Arc::new(session_source),
        connection: connection.clone(),
        attachment: attachment.clone(),
        when_attached: Arc::new(move |session_id| {
            let (binding, attachment) = (session_binding.clone(), attachment.clone());
            Box::pin(async move {
                binding.rebind(true, &BACKGROUND_CONTEXT).await?;
                binding.ready(&BACKGROUND_CONTEXT).await?;
                publish(&attachment, json!({ "status": "attached", "sessionId": session_id }));
                Ok(())
            })
        }),
    };
    let terminal = VirtualTerminal::shared(100, 30);
    let options = TuiOptions { env: Some(Rc::new(MapEnv::default())), ..TuiOptions::default() };
    let tui = Tui::new_alt_screen(terminal, options, TuiAltScreenOptions::default());
    let (finished, renders) = (Rc::new(Cell::new(false)), Rc::new(Cell::new(0usize)));
    let (done, rendered) = (finished.clone(), renders.clone());
    let ui = ExperimentalClientTui::create(ClientTuiOptions {
        command: command.clone(),
        tui,
        servers: vec![tui_server],
        facet_loader: None,
        request_render: Rc::new(move || rendered.set(rendered.get() + 1)),
        finish: Rc::new(move || done.set(true)),
    })
    .await
    .unwrap();

    assert_eq!(*calls.creates.lock().unwrap(), creates, "{kind}");
    let package_paths =
        command.plugin_packages.as_ref().map(|paths| paths.iter().map(|path| resolve_path(path)).collect());
    assert_eq!(
        *calls.prepared.lock().unwrap(),
        [PrepareSessionRequest { session_id: session_id.into(), package_paths }]
    );
    assert_eq!(mock.attachment.value().to_serde(), json!({ "status": "attached", "sessionId": session_id }));
    assert!(calls.selected.lock().unwrap().is_empty());
    wait_for("the footer", || screen(&ui).contains("faux/faux-1")).await;
    for expected in [format!("Server: {SERVER_ID}"), format!("Session: {session_id}")] {
        assert!(screen(&ui).contains(&expected), "{kind}: missing {expected}");
    }
    assert!(!screen(&ui).contains("Experimental Sessions") && !screen(&ui).contains("Experimental Models"));

    // Startup selection is the only behavior specific to continue and plugin-selected Sessions.
    if kind == "new" {
        exercise_lifecycle(&ui, &calls, &mock, &connection, &prompts, &finish_prompt, &finished, &renders).await;
    }
    ui.close().await.unwrap();
    server_binding.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    transcript_state.dispose().unwrap();
    harness.close(&BACKGROUND_CONTEXT).await.unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn exercise_lifecycle(
    ui: &Rc<ExperimentalClientTui>,
    calls: &Calls,
    mock: &Mock,
    connection: &MutableReplicatedState,
    prompts: &Mutex<Vec<String>>,
    finish_prompt: &Notify,
    finished: &Cell<bool>,
    renders: &Cell<usize>,
) {
    ui.handle_input("hello");
    ui.handle_input("\r");
    wait_for("the prompt", || *prompts.lock().unwrap() == ["hello"]).await;
    wait_for("the working status", || screen(ui).contains("Working...")).await;
    finish_prompt.notify_one();
    wait_for("the answer", || screen(ui).contains("remote answer")).await;
    assert!(screen(ui).contains("hello"));
    assert!(!screen(ui).contains("Working..."));

    for input in ["/reload", "\u{1b}", "\r"] {
        ui.handle_input(input);
    }
    wait_for("both reloads", || {
        *calls.presentation_reloads.lock().unwrap() == 1 && *calls.session_reloads.lock().unwrap() == 1
    })
    .await;
    wait_for("the reload status", || screen(ui).contains("Reloaded plugins.")).await;

    publish(&mock.attachment, json!({ "status": "detached" }));
    let lost = json!({ "status": "disconnected", "since": "later", "reason": "network lost", "retryAt": null });
    publish(connection, lost);
    wait_for("the retry status", || screen(ui).contains("retrying")).await;
    for exit in ["\u{3}", "\u{4}"] {
        ui.handle_input(exit);
        assert!(finished.replace(false), "{exit:?} must finish while reconnecting");
    }
    publish(connection, json!({ "status": "connecting", "attempt": 1 }));
    publish(connection, json!({ "status": "connected", "since": "reconnected" }));
    publish(&mock.attachment, json!({ "status": "attached", "sessionId": "two" }));
    wait_for("the reattached Session", || !ui.state.borrow().busy).await;
    assert!(!screen(ui).contains("Reattaching"));

    for input in ["/model", "\u{1b}", "\r"] {
        ui.handle_input(input);
    }
    wait_for("the model selector", || screen(ui).contains("Select model:")).await;
    ui.handle_input("\u{1b}[B");
    ui.handle_input("\r");
    let two = ModelRef { provider: "test".into(), model_id: "two".into() };
    wait_for("the model selection", || *calls.selected.lock().unwrap() == [two.clone()]).await;
    assert_eq!(
        mock.models.value().to_serde()["configuration"]["model"],
        json!({ "provider": "test", "modelId": "two" })
    );
    wait_for("the closed model selector", || !screen(ui).contains("Select model:")).await;

    for input in ["/thinking", "\u{1b}", "\r"] {
        ui.handle_input(input);
    }
    wait_for("the thinking selector", || screen(ui).contains("Select thinking level:")).await;
    ui.handle_input("\u{1b}[B");
    ui.handle_input("\r");
    wait_for("the thinking selection", || *calls.thinking.lock().unwrap() == [ThinkingLevel::High]).await;
    assert_eq!(mock.models.value().to_serde()["configuration"]["thinkingLevel"], json!("high"));

    ui.handle_input("\u{3}");
    wait_for("the exit", || finished.get()).await;

    ui.close().await.unwrap();
    let renders_after_close = renders.get();
    publish(&mock.directory, json!({ "revision": 3, "sessions": [] }));
    publish(&mock.attachment, json!({ "status": "detached" }));
    let mut models = mock.models.value().to_serde();
    models["refresh"] = json!({ "status": "refreshing" });
    publish(&mock.models, models);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(renders.get(), renders_after_close);
}

#[tokio::test(flavor = "multi_thread")]
async fn opens_a_session_directly_and_exercises_the_full_lifecycle_only_for_a_new_session() {
    let _keys = setup();
    let plugin = ClientCommand { plugin_packages: Some(vec!["./example-plugin".into()]), ..ClientCommand::default() };
    let cases = [
        ("new", ClientCommand::default(), "two", 1),
        ("continued", ClientCommand { continue_session: true, ..ClientCommand::default() }, "one", 0),
        ("plugin-selected", plugin, "two", 1),
    ];
    for (kind, command, session_id, creates) in cases {
        tokio::task::LocalSet::new().run_until(run_case(kind, command, session_id, creates)).await;
    }
}
