//! Port of upstream `test/fixtures/faux-session-worker.ts` and `fixtures/keyed-service.ts`. Upstream points
//! the worker role at another entry module; here a wrapper script reruns this test binary with only the
//! `faux_worker_entry` test selected, and that test runs the worker when the role is set.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use pi_chord::facets::{FacetEnvironment, ServiceSpawner, create_static_facet_loader, define_facet};
use pi_chord::services::{ServiceImpl, ServiceRef};
use pi_chord::{BACKGROUND_CONTEXT, ChordError, Json, Unsubscribe};
use pi_coding_agent::experimental::process::{
    InternalProcessRole, consume_internal_process_role, set_internal_process_program,
};
use pi_coding_agent::experimental::services::worker::SessionWorkerRuntime;
use pi_coding_agent::experimental::session_worker_harness::create_coding_agent_harness;
use pi_coding_agent::experimental::session_worker_run::run_session_worker_with_harness;
use pi_durable::harness::types::{AgentChange, Change, ModelRef};
use pi_durable::harness::{CreateOptions, Harness, HarnessOptions, RegistryReader, create_registry};
use pi_durable::storage::open_sqlite_storage;
use pi_durable::testing::faux::{
    FauxMessageOptions, RegisterFauxProviderOptions, faux_assistant_message, faux_provider,
};

pub static KEYED_PROBE: LazyLock<ServiceRef> = LazyLock::new(|| ServiceRef::remote("test.keyed-probe"));

/// Run coordinators and servers from the `pi` binary and Session workers from the faux worker entry.
pub fn install() {
    static SCRIPT: OnceLock<PathBuf> = OnceLock::new();
    let script = SCRIPT.get_or_init(|| {
        let directory = tempfile::Builder::new().prefix("pi-faux-worker-").tempdir().unwrap().keep();
        let script = directory.join("faux-worker.sh");
        let binary = std::env::current_exe().unwrap();
        let body = format!(
            "#!/bin/sh\nexec '{}' --exact faux_worker::faux_worker_entry --test-threads=1 --nocapture \"$@\"\n",
            binary.display()
        );
        std::fs::write(&script, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    });
    set_internal_process_program(None, Some(PathBuf::from(env!("CARGO_BIN_EXE_pi"))));
    set_internal_process_program(Some(InternalProcessRole::SessionWorker), Some(script.clone()));
}

#[test]
fn faux_worker_entry() {
    if consume_internal_process_role().ok().flatten() != Some(InternalProcessRole::SessionWorker) {
        return;
    }
    let options = std::env::args().next_back().unwrap_or_default();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    if let Some(agent_dir) = real_agent_dir(&options) {
        let result =
            runtime.block_on(run_session_worker_with_harness(&[options], create_coding_agent_harness(agent_dir)));
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }
    let result = runtime.block_on(run_session_worker_with_harness(
        &[options],
        Arc::new(|path, options| {
            Box::pin(async move {
                if options.provider.as_deref() != Some("anthropic")
                    || options.model.as_deref() != Some("claude-sonnet-4-5")
                {
                    let message = format!("Unexpected faux worker model: {:?}/{:?}", options.provider, options.model);
                    return Err(ChordError::error(message));
                }
                let faux = faux_provider(RegisterFauxProviderOptions::default());
                let answer_options = FauxMessageOptions { timestamp: Some(20), ..FauxMessageOptions::default() };
                let answer = faux_assistant_message("deterministic remote answer", answer_options);
                faux.set_responses([answer.clone(), answer]);
                let registry: Arc<dyn RegistryReader> = Arc::new(create_registry());
                let storage = Arc::new(open_sqlite_storage(&path.display().to_string()).await?);
                let harness =
                    Harness::open(storage, HarnessOptions::new(faux.provider(), registry), &BACKGROUND_CONTEXT).await?;
                let model = faux.get_model();
                let mut agent =
                    AgentChange::model(ModelRef { provider: model.provider.clone(), model_id: model.id.clone() });
                agent.cwd = Change::Set(options.metadata.cwd.clone());
                let conversation =
                    harness.root(&BACKGROUND_CONTEXT, CreateOptions::default().with_agent(agent)).await?;
                let loader = create_static_facet_loader(vec![define_facet("@test/keyed-probe", keyed_probe_setup)]);
                Ok(SessionWorkerRuntime {
                    harness,
                    conversation,
                    model_catalog: None,
                    model_defaults: None,
                    facet_loader: Some(loader),
                    cleanup: None,
                })
            })
        }),
    ));
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}

/// Prefix of agent directories whose Sessions run the real coding agent worker instead of the faux one.
pub const REAL_AGENT_PREFIX: &str = "pi-real-agent-";

/// The agent directory of a real-worker Session: Rust tests cannot stub `PI_CODING_AGENT_DIR`, so the worker
/// takes it from its Session directory, `<agent dir>/experimental/sessions`.
fn real_agent_dir(options: &str) -> Option<PathBuf> {
    let options: serde_json::Value = serde_json::from_str(options).ok()?;
    let sessions = PathBuf::from(options["sessionDir"].as_str()?);
    let agent_dir = sessions.parent()?.parent()?.to_path_buf();
    let name = agent_dir.file_name()?.to_string_lossy().into_owned();
    name.starts_with(REAL_AGENT_PREFIX).then_some(agent_dir)
}

fn keyed_probe_setup(env: &FacetEnvironment) -> pi_chord::ChordResult<()> {
    let probes = env.provide_many(&KEYED_PROBE)?;
    let activation_env = env.clone();
    env.on_activate(move || {
        let (env, probes) = (activation_env.clone(), probes.clone());
        async move { spawn_probe(&env, &probes, "first") }
    })
}

fn spawn_probe(env: &FacetEnvironment, probes: &ServiceSpawner, value: &str) -> pi_chord::ChordResult<()> {
    let state = env.replicated_state(Json::from(&serde_json::json!({ "value": value })))?;
    let close: Arc<Mutex<Option<Unsubscribe>>> = Arc::default();
    let (replace_env, replace_probes, replace_close) = (env.clone(), probes.clone(), close.clone());
    let implementation = ServiceImpl::new()
        .state("state", &state)
        .method("replace", move |args: Vec<Json>, _context| {
            let (env, probes, close) = (replace_env.clone(), replace_probes.clone(), replace_close.clone());
            async move {
                if let Some(close) = close.lock().unwrap().take() {
                    close.close()?;
                }
                let next = args.first().and_then(Json::as_str).unwrap_or_default().to_string();
                spawn_probe(&env, &probes, &next)?;
                Ok(None)
            }
        })
        .method("wait", |_args: Vec<Json>, context| async move {
            let signal = context.abort_signal().ok_or_else(|| ChordError::error("Probe wait requires cancellation"))?;
            Err(signal.cancelled().await)
        });
    *close.lock().unwrap() = Some(probes.spawn("probe", implementation)?);
    Ok(())
}
