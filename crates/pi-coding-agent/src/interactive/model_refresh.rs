//! Catalog refreshes started from `/model`, `/scoped-models`, and `/login` (upstream
//! `model-catalog-refresh.ts`, `findExactModelMatch`, and `completeProviderAuthentication`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
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

type SharedRefresh = Shared<BoxFuture<'static, Result<RefreshResult, String>>>;

/// One in-flight refresh of a model runtime, keyed by the runtime's address.
struct ActiveRefresh {
    generation: u64,
    cancel: CancellationToken,
    result: SharedRefresh,
    waiters: usize,
}

static ACTIVE: LazyLock<Mutex<HashMap<usize, ActiveRefresh>>> = LazyLock::new(Default::default);
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Leaves a shared refresh; the last caller to leave before it finishes cancels it.
struct Waiter {
    key: usize,
    generation: u64,
}

impl Drop for Waiter {
    fn drop(&mut self) {
        let mut active = ACTIVE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(entry) = active.get_mut(&self.key).filter(|entry| entry.generation == self.generation) else {
            return;
        };
        entry.waiters -= 1;
        if entry.waiters == 0 {
            entry.cancel.cancel();
            active.remove(&self.key);
        }
    }
}

/// Reload `models.json` and refresh every catalog. Concurrent callers share one refresh, and
/// each caller's `cancel` only ends its own wait (upstream `refreshModelCatalogs`).
async fn shared_refresh(runtime: Arc<ModelRuntime>, cancel: CancellationToken) -> Result<RefreshResult, String> {
    let key = Arc::as_ptr(&runtime) as usize;
    join_refresh(key, cancel, move |token| {
        let options = RefreshOptions { allow_network: !crate::config::offline(), cancel: token, ..Default::default() };
        let task = tokio::spawn(async move { runtime.refresh_catalogs_with(options).await });
        async move { task.await.map_err(|error| error.to_string()) }.boxed()
    })
    .await
}

/// Wait for the refresh running under `key`, starting one with `start` when none runs.
async fn join_refresh(
    key: usize,
    cancel: CancellationToken,
    start: impl FnOnce(CancellationToken) -> BoxFuture<'static, Result<RefreshResult, String>>,
) -> Result<RefreshResult, String> {
    let (result, waiter) = {
        let mut active = ACTIVE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = active.entry(key).or_insert_with(|| {
            let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
            let token = CancellationToken::new();
            let operation = start(token.clone());
            let result = async move {
                let result = operation.await;
                let mut active = ACTIVE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if active.get(&key).is_some_and(|entry| entry.generation == generation) {
                    active.remove(&key);
                }
                result
            };
            ActiveRefresh { generation, cancel: token, result: result.boxed().shared(), waiters: 0 }
        });
        entry.waiters += 1;
        (entry.result.clone(), Waiter { key, generation: entry.generation })
    };
    let outcome = tokio::select! {
        result = result => result,
        () = cancel.cancelled() => Ok(RefreshResult { aborted: true, ..RefreshResult::default() }),
    };
    drop(waiter);
    outcome
}

/// Reload `models.json` and refresh every catalog, giving up after 15 s or when `cancel` fires.
pub(super) async fn refresh_catalogs(runtime: Arc<ModelRuntime>, cancel: CancellationToken) -> RefreshOutcome {
    let token = cancel.child_token();
    tokio::select! {
        result = shared_refresh(runtime, token.clone()) => match result {
            Ok(result) => RefreshOutcome::Done(result),
            Err(error) => RefreshOutcome::Failed(error),
        },
        () = tokio::time::sleep(MODEL_REFRESH_TIMEOUT) => {
            token.cancel();
            RefreshOutcome::TimedOut
        }
    }
}

/// Refresh one provider's catalog after `/login`, with the 15 s limit. Forced, so a new key's
/// model list replaces the one saved for the previous key.
pub(super) async fn refresh_after_login(runtime: Arc<ModelRuntime>, provider: String) -> RefreshOutcome {
    let cancel = CancellationToken::new();
    let allow_network = !crate::config::offline();
    let options =
        RefreshOptions { allow_network, providers: Some(vec![provider]), force: true, cancel: cancel.clone() };
    let task = tokio::spawn(async move { runtime.refresh_catalogs_with(options).await });
    tokio::select! {
        result = task => match result {
            Ok(result) => RefreshOutcome::Done(result),
            Err(error) => RefreshOutcome::Failed(error.to_string()),
        },
        () = tokio::time::sleep(MODEL_REFRESH_TIMEOUT) => {
            cancel.cancel();
            RefreshOutcome::TimedOut
        }
    }
}

/// The warning after a login whose catalog refresh did not finish (upstream wording).
pub(super) fn login_refresh_warning(outcome: &RefreshOutcome, action: &str) -> Option<String> {
    match outcome {
        RefreshOutcome::Done(result) if result.aborted => {
            Some(format!("{action}, but its model catalog refresh timed out; using cached models."))
        }
        RefreshOutcome::TimedOut => {
            Some(format!("{action}, but its model catalog refresh timed out; using cached models."))
        }
        RefreshOutcome::Done(result) if !result.errors.is_empty() => {
            Some(format!("{action}, but its model catalog could not be refreshed; using cached models."))
        }
        RefreshOutcome::Done(_) => None,
        RefreshOutcome::Failed(error) => {
            Some(format!("{action}, but its model catalog could not be refreshed: {error}"))
        }
    }
}

/// The refresh the model selector runs while it is open.
pub(super) fn selector_refresh(runtime: Arc<ModelRuntime>) -> ModelRefreshFn {
    Box::new(move |cancel| {
        Box::pin(async move {
            let result = shared_refresh(runtime, cancel).await?;
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A refresh that counts its starts and finishes when `release` fires or its token is cancelled.
    fn counting(
        starts: &Arc<AtomicUsize>,
        release: &CancellationToken,
        cancelled: &Arc<Mutex<Option<CancellationToken>>>,
    ) -> impl FnOnce(CancellationToken) -> BoxFuture<'static, Result<RefreshResult, String>> + use<> {
        let (starts, release, cancelled) = (starts.clone(), release.clone(), cancelled.clone());
        move |token| {
            starts.fetch_add(1, Ordering::SeqCst);
            *cancelled.lock().unwrap() = Some(token.clone());
            async move {
                tokio::select! {
                    () = release.cancelled() => Ok(RefreshResult::default()),
                    () = token.cancelled() => Ok(RefreshResult { aborted: true, ..RefreshResult::default() }),
                }
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh_and_cancel_only_their_own_wait() {
        let (starts, release, token) = (Arc::new(AtomicUsize::new(0)), CancellationToken::new(), Arc::default());
        let (first_cancel, second_cancel) = (CancellationToken::new(), CancellationToken::new());
        let first = tokio::spawn(join_refresh(1001, first_cancel.clone(), counting(&starts, &release, &token)));
        tokio::task::yield_now().await;
        let second = tokio::spawn(join_refresh(1001, second_cancel, counting(&starts, &release, &token)));
        tokio::task::yield_now().await;
        first_cancel.cancel();
        assert!(first.await.unwrap().unwrap().aborted);
        assert!(!token.lock().unwrap().as_ref().unwrap().is_cancelled(), "another caller still waits");
        release.cancel();
        assert!(!second.await.unwrap().unwrap().aborted);
        assert_eq!(starts.load(Ordering::SeqCst), 1);

        let again = join_refresh(1001, CancellationToken::new(), counting(&starts, &release, &token)).await;
        assert!(!again.unwrap().aborted);
        assert_eq!(starts.load(Ordering::SeqCst), 2, "a finished refresh is not reused");
    }

    #[tokio::test]
    async fn the_last_caller_to_leave_cancels_the_refresh() {
        let (starts, release, token) = (Arc::new(AtomicUsize::new(0)), CancellationToken::new(), Arc::default());
        let cancel = CancellationToken::new();
        let waiting = tokio::spawn(join_refresh(1002, cancel.clone(), counting(&starts, &release, &token)));
        tokio::task::yield_now().await;
        cancel.cancel();
        assert!(waiting.await.unwrap().unwrap().aborted);
        assert!(token.lock().unwrap().as_ref().unwrap().is_cancelled());
        assert!(!ACTIVE.lock().unwrap().contains_key(&1002));
    }
}
