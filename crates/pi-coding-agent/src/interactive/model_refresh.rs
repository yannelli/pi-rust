//! Catalog refreshes started from `/model` and `/scoped-models` (upstream
//! `model-catalog-refresh.ts` and `findExactModelMatch`).

use std::sync::Arc;

use pi_ai::Model;
use tokio_util::sync::CancellationToken;

use super::mode::InteractiveMode;
use crate::agent_session::ModelRuntime;
use crate::catalog::{MODEL_REFRESH_TIMEOUT, RefreshOptions, RefreshResult};
use crate::interactive_components::model_selector::{ModelRefreshFn, ModelsRefreshResult};
use crate::model_resolver::find_exact_model_reference_match;

/// How a refresh with the 15 s limit ended.
pub(super) enum RefreshOutcome {
    Done(RefreshResult),
    TimedOut,
    Failed(String),
}

impl RefreshOutcome {
    /// The warning to show, ending in `suffix` such as `searching cached models`.
    pub(super) fn warning(&self, suffix: &str) -> Option<String> {
        match self {
            Self::Done(result) if result.aborted => Some(format!("Model refresh timed out; {suffix}.")),
            Self::Done(result) if !result.errors.is_empty() => {
                let providers: Vec<&str> = result.errors.keys().map(String::as_str).collect();
                Some(format!("Could not refresh {}; {suffix}.", providers.join(", ")))
            }
            Self::Done(_) => None,
            Self::TimedOut => Some(format!("Model refresh timed out; {suffix}.")),
            Self::Failed(error) => Some(format!("Could not refresh model catalogs: {error}")),
        }
    }
}

/// Reload `models.json` and refresh every catalog, giving up after 15 s or when `cancel` fires.
pub(super) async fn refresh_catalogs(runtime: Arc<ModelRuntime>, cancel: CancellationToken) -> RefreshOutcome {
    let token = cancel.child_token();
    let options =
        RefreshOptions { allow_network: !crate::config::offline(), cancel: token.clone(), ..Default::default() };
    let task = tokio::spawn(async move { runtime.refresh_catalogs_with(options).await });
    tokio::select! {
        result = task => match result {
            Ok(result) => RefreshOutcome::Done(result),
            Err(error) => RefreshOutcome::Failed(error.to_string()),
        },
        () = tokio::time::sleep(MODEL_REFRESH_TIMEOUT) => {
            token.cancel();
            RefreshOutcome::TimedOut
        }
    }
}

/// The refresh the model selector runs while it is open.
pub(super) fn selector_refresh(runtime: Arc<ModelRuntime>) -> ModelRefreshFn {
    Box::new(move |cancel| {
        Box::pin(async move {
            let options = RefreshOptions { allow_network: !crate::config::offline(), cancel, ..Default::default() };
            let result = tokio::spawn(async move { runtime.refresh_catalogs_with(options).await })
                .await
                .map_err(|error| error.to_string())?;
            Ok(ModelsRefreshResult { aborted: result.aborted, errors: result.errors.into_keys().collect() })
        })
    })
}

impl InteractiveMode {
    /// An exact match among the scoped models, or the available ones after a catalog refresh.
    async fn find_exact_model_match(&self, search: &str) -> Option<Model> {
        let session = self.session();
        let scoped = session.scoped_models();
        if !scoped.is_empty() {
            let models: Vec<Model> = scoped.into_iter().map(|scoped| scoped.model).collect();
            return find_exact_model_reference_match(search, &models);
        }
        let runtime = session.model_runtime().clone();
        if let Some(model) = find_exact_model_reference_match(search, &runtime.available_models()) {
            return Some(model);
        }
        self.show_status("Refreshing model catalogs…");
        let outcome = refresh_catalogs(runtime.clone(), CancellationToken::new()).await;
        if let Some(warning) = outcome.warning("searching cached models") {
            self.show_warning(&warning);
        }
        find_exact_model_reference_match(search, &runtime.available_models())
    }

    /// `/model [reference]`: switch to an exact match, else open the selector with the search.
    pub(super) fn handle_model_command(&self, search: Option<String>) {
        let Some(search) = search else { return self.show_model_selector(None) };
        self.spawn(move |mode| async move {
            match mode.find_exact_model_match(&search).await {
                Some(model) => mode.apply_model(model, false),
                None => mode.show_model_selector(Some(search)),
            }
        });
    }
}
