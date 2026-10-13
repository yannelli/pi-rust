//! The `/llama` command flow behind a UI trait: list router models, load or unload one, and
//! download from Hugging Face with progress and cancellation (upstream `extensions/llama/index.ts`
//! and `runWithProgress` in `ui.ts`).

use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use pi_ai::auth::ResolvedAuth;
use tokio_util::sync::CancellationToken;

use super::auth::ENV_LLAMA_BASE_URL;
use super::client::{LlamaClient, LlamaError, LlamaModelInfo, LlamaProgress, format_bytes, status};
use super::huggingface::{GatedAccess, HuggingFaceClient};
use super::provider::{LLAMA_PROVIDER_ID, LlamaProvider};
use super::view::{ProgressState, parse_hugging_face_model};

const SYNC_TIMEOUT: Duration = Duration::from_millis(15_000);

#[derive(Clone, Debug, PartialEq)]
pub enum LlamaManagerAction {
    Model(Box<LlamaModelInfo>),
    Download,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionChoice {
    Retry,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyLevel {
    Info,
    Warning,
    Error,
}

/// The screens `/llama` needs; the interactive mode implements it.
#[async_trait]
pub trait LlamaUi: Send + Sync {
    async fn show_models(&self, server_url: &str, models: &[LlamaModelInfo]) -> LlamaManagerAction;
    /// The chosen option, or `None` when dismissed.
    async fn select(&self, title: &str, options: &[String]) -> Option<String>;
    async fn confirm(&self, title: &str, message: &str) -> bool;
    async fn connection_error(&self, server_url: &str, message: &str) -> ConnectionChoice;
    /// Search Hugging Face with `client`; the chosen `owner/repository[:quant]`.
    async fn search_models(&self, client: &HuggingFaceClient) -> Option<String>;
    fn show_status(&self, title: &str, message: &str);
    /// Show `state` and wait until the user asks to stop.
    async fn progress(&self, state: &ProgressState);
    fn update_progress(&self, state: &ProgressState);
    fn notify(&self, message: &str, level: NotifyLevel);
}

/// Refreshes the llama.cpp models in the model registry, with network access even offline,
/// since `/llama` already reached the router.
#[async_trait]
pub trait LlamaCatalogSync: Send + Sync {
    async fn refresh_llama_models(&self) -> Result<(), String>;
}

pub type ProgressFn<'a> = Arc<dyn Fn(LlamaProgress) + Send + Sync + 'a>;

#[derive(Clone, Debug, PartialEq)]
pub enum ProgressOutcome<T> {
    Done(T),
    Cancelled,
}

pub struct ProgressOptions {
    pub title: String,
    pub model: String,
    pub initial_message: String,
    pub cancel_title: String,
    pub cancel_message: String,
}

/// Run `run` with a progress view. Stopping asks for confirmation, calls `cancel` (for example
/// an unload), and then cancels `run`.
pub async fn run_with_progress<'a, T, Run, RunFut, Cancel, CancelFut>(
    ui: &'a dyn LlamaUi,
    options: ProgressOptions,
    run: Run,
    cancel: Cancel,
) -> Result<ProgressOutcome<T>, LlamaError>
where
    Run: FnOnce(CancellationToken, ProgressFn<'a>) -> RunFut,
    RunFut: Future<Output = Result<T, LlamaError>>,
    Cancel: FnOnce() -> CancelFut,
    CancelFut: Future<Output = Result<(), LlamaError>>,
{
    let controller = CancellationToken::new();
    let initial = LlamaProgress::message(&options.initial_message);
    let state = Arc::new(Mutex::new(ProgressState { title: options.title, model: options.model, progress: initial }));
    let shared = state.clone();
    let update: ProgressFn<'a> = Arc::new(move |progress: LlamaProgress| {
        let snapshot = {
            let mut state = shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.progress.message = progress.message;
            if progress.ratio.is_some() {
                state.progress.ratio = progress.ratio;
            }
            if progress.detail.is_some() {
                state.progress.detail = progress.detail;
            }
            state.clone()
        };
        ui.update_progress(&snapshot);
    });
    let mut settled = pin!(run(controller.clone(), update).fuse());
    let snapshot = || state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    let mut outcome = None;
    while outcome.is_none() {
        let current = snapshot();
        tokio::select! {
            result = &mut settled => { outcome = Some(result); break; }
            _ = ui.progress(&current) => {}
        }
        let mut confirm = pin!(ui.confirm(&options.cancel_title, &options.cancel_message));
        let stop = loop {
            tokio::select! {
                stop = &mut confirm => break stop,
                result = &mut settled, if outcome.is_none() => outcome = Some(result),
            }
        };
        if !stop || outcome.is_some() {
            continue;
        }
        let cancelled = cancel().await;
        controller.cancel();
        cancelled?;
        let _ = settled.await;
        return Ok(ProgressOutcome::Cancelled);
    }
    outcome.expect("loop ends with an outcome").map(ProgressOutcome::Done)
}

/// "Could not connect to the server." for connection failures, else the error text.
pub fn connection_error_message(error: &LlamaError) -> String {
    if error.is_connection_error() { "Could not connect to the server.".into() } else { error.to_string() }
}

/// A client for the configured router, or the notice to show when llama.cpp is not configured.
pub fn configured_llama_client(auth: Option<&ResolvedAuth>) -> Result<LlamaClient, String> {
    let Some(auth) = auth else { return Err(format!("Configure llama.cpp with /login {LLAMA_PROVIDER_ID}")) };
    let configured = auth.env.as_ref().and_then(|env| env.get(ENV_LLAMA_BASE_URL)).filter(|url| !url.is_empty());
    let url = configured.cloned().or_else(|| auth.auth.base_url.clone()).unwrap_or_default();
    LlamaClient::new(&url, auth.auth.api_key.clone())
}

pub struct LlamaManager<'a> {
    pub client: LlamaClient,
    pub provider: &'a LlamaProvider,
    pub ui: &'a dyn LlamaUi,
    pub sync: &'a dyn LlamaCatalogSync,
    pub hugging_face: HuggingFaceClient,
}

impl LlamaManager<'_> {
    /// Show the given or a fresh router catalog in `/model` and the registry.
    pub async fn sync_catalog(&self, catalog: Option<Vec<LlamaModelInfo>>) -> Result<Vec<LlamaModelInfo>, LlamaError> {
        let cancel = CancellationToken::new();
        let current = match catalog {
            Some(catalog) => catalog,
            None => match tokio::time::timeout(SYNC_TIMEOUT, self.client.list(false, &cancel)).await {
                Ok(result) => result?,
                Err(_) => return Err(LlamaError::Connection("Model catalog refresh timed out.".into())),
            },
        };
        self.provider.set_catalog(&current, self.client.server_url(), false);
        match tokio::time::timeout(SYNC_TIMEOUT, self.sync.refresh_llama_models()).await {
            Ok(Ok(())) => Ok(current),
            Ok(Err(error)) => Err(LlamaError::Server(error)),
            Err(_) => Err(LlamaError::Connection("Model catalog refresh timed out.".into())),
        }
    }

    async fn read_catalog(&self) -> Option<Vec<LlamaModelInfo>> {
        loop {
            match self.sync_catalog(None).await {
                Ok(catalog) => return Some(catalog),
                Err(error) => {
                    let message = connection_error_message(&error);
                    if self.ui.connection_error(self.client.server_url(), &message).await == ConnectionChoice::Close {
                        return None;
                    }
                }
            }
        }
    }

    /// The `/llama` loop: show models until closed and apply the chosen action.
    pub async fn run(&self) {
        let Some(mut catalog) = self.read_catalog().await else { return };
        loop {
            let action = self.ui.show_models(self.client.server_url(), &catalog).await;
            let result = match action {
                LlamaManagerAction::Close => return,
                LlamaManagerAction::Download => self.download_model().await,
                LlamaManagerAction::Model(model) if model.is_loaded() => self.unload_model(&model).await,
                LlamaManagerAction::Model(model) if model.is_status(status::UNLOADED) => {
                    self.load_model(&catalog, &model).await
                }
                LlamaManagerAction::Model(model) => {
                    self.ui.notify(&format!("{} is {}", model.id, model.status.value), NotifyLevel::Warning);
                    Ok(())
                }
            };
            let Some(refreshed) = self.read_catalog().await else { return };
            catalog = refreshed;
            if let Err(error) = result
                && !error.is_connection_error()
            {
                self.ui.notify(&error.to_string(), NotifyLevel::Error);
            }
        }
    }

    async fn restore_loaded(&self, loaded: &[LlamaModelInfo]) -> Result<(), LlamaError> {
        self.ui.notify("Restoring previously loaded models", NotifyLevel::Info);
        let cancel = CancellationToken::new();
        for model in loaded {
            self.client.load_and_wait(&model.id, &|_| {}, &cancel).await?;
        }
        self.sync_catalog(None).await.map(drop)
    }

    /// Load `target`, first asking whether other loaded models stay loaded.
    pub async fn load_model(&self, catalog: &[LlamaModelInfo], target: &LlamaModelInfo) -> Result<(), LlamaError> {
        let loaded: Vec<LlamaModelInfo> =
            catalog.iter().filter(|model| model.id != target.id && model.is_loaded()).cloned().collect();
        let mut replace = false;
        if !loaded.is_empty() {
            let title = match loaded.len() {
                1 => "1 model is loaded".to_string(),
                count => format!("{count} models are loaded"),
            };
            let options = ["Unload all and load", "Keep loaded and load", "Cancel"].map(String::from);
            let choice = self.ui.select(&title, &options).await;
            let Some(choice) = choice.filter(|choice| choice != "Cancel") else { return Ok(()) };
            replace = choice == "Unload all and load";
        }
        if replace {
            let cancel = CancellationToken::new();
            for model in &loaded {
                self.client.unload_and_wait(&model.id, &cancel).await?;
            }
        }
        let options = ProgressOptions {
            title: "Loading model".into(),
            model: target.id.clone(),
            initial_message: "Starting…".into(),
            cancel_title: "Stop loading?".into(),
            cancel_message: target.id.clone(),
        };
        let client = &self.client;
        let unload_token = CancellationToken::new();
        let outcome = run_with_progress(
            self.ui,
            options,
            |signal, update| async move { client.load_and_wait(&target.id, &*update, &signal).await },
            || client.unload(&target.id, &unload_token),
        )
        .await;
        match outcome {
            Ok(ProgressOutcome::Cancelled) if replace => self.restore_loaded(&loaded).await,
            Ok(ProgressOutcome::Cancelled) => Ok(()),
            Ok(ProgressOutcome::Done(_)) => {
                let refreshed = self.sync_catalog(None).await?;
                let now_loaded = refreshed.iter().any(|model| model.id == target.id && model.is_status(status::LOADED));
                let message = if now_loaded { "Loaded" } else { "Load started for" };
                self.ui.notify(&format!("{message} {}", target.id), NotifyLevel::Info);
                Ok(())
            }
            Err(error) => {
                if replace {
                    let _ = self.restore_loaded(&loaded).await;
                }
                Err(error)
            }
        }
    }

    pub async fn unload_model(&self, model: &LlamaModelInfo) -> Result<(), LlamaError> {
        if !self.ui.confirm("Unload model?", &model.id).await {
            return Ok(());
        }
        self.client.unload_and_wait(&model.id, &CancellationToken::new()).await?;
        self.sync_catalog(None).await?;
        self.ui.notify(&format!("Unloaded {}", model.id), NotifyLevel::Info);
        Ok(())
    }

    /// Search Hugging Face, warn about gated access, pick a quantization, and download.
    pub async fn download_model(&self) -> Result<(), LlamaError> {
        let Some(selected) = self.ui.search_models(&self.hugging_face).await else { return Ok(()) };
        let (repository, mut quantization) = parse_hugging_face_model(&selected);
        self.ui.show_status("Loading model details", &repository);
        let details = self.hugging_face.details(&repository, &CancellationToken::new()).await?;
        if details.gated != GatedAccess::Open {
            let approval = match details.gated {
                GatedAccess::Manual => "Manual approval is required",
                _ => "Accept the access terms",
            };
            let title = format!(
                "Hugging Face access required\n{id}\n\n{approval} at:\nhttps://huggingface.co/{id}\n\n\
                 The llama.cpp server needs HF_TOKEN with access.",
                id = details.id
            );
            let options = ["Continue", "Back"].map(String::from);
            if self.ui.select(&title, &options).await.as_deref() != Some("Continue") {
                return Ok(());
            }
        }
        if quantization.is_none() && !details.quantizations.is_empty() {
            let options: Vec<String> = details.quantizations.iter().map(quantization_option).collect();
            let Some(choice) = self.ui.select(&format!("Select quantization\n{}", details.id), &options).await else {
                return Ok(());
            };
            let Some(index) = options.iter().position(|option| *option == choice) else { return Ok(()) };
            quantization = Some(details.quantizations[index].name.clone());
        }
        let model = match quantization {
            Some(quantization) => format!("{}:{quantization}", details.id),
            None => details.id.clone(),
        };
        let options = ProgressOptions {
            title: "Downloading model".into(),
            model: model.clone(),
            initial_message: "Starting…".into(),
            cancel_title: "Stop download?".into(),
            cancel_message: model.clone(),
        };
        let (client, target) = (&self.client, model.as_str());
        let unload_token = CancellationToken::new();
        let outcome = run_with_progress(
            self.ui,
            options,
            |signal, update| async move { client.download_and_wait(target, &*update, &signal).await },
            || client.unload(target, &unload_token),
        )
        .await?;
        let ProgressOutcome::Done(catalog) = outcome else { return Ok(()) };
        self.sync_catalog(Some(catalog)).await?;
        self.ui.notify(&format!("Downloaded {model}"), NotifyLevel::Info);
        Ok(())
    }
}

/// `Q4_K_M · 4.37 GiB · recommended`.
pub fn quantization_option(entry: &super::huggingface::HuggingFaceQuantization) -> String {
    let mut details = Vec::new();
    if let Some(size) = entry.size {
        details.push(format_bytes(size as f64));
    }
    if entry.name == "Q4_K_M" {
        details.push("recommended".into());
    }
    if details.is_empty() { entry.name.clone() } else { format!("{} · {}", entry.name, details.join(" · ")) }
}
