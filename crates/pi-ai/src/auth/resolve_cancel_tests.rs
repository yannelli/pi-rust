use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tokio::sync::Notify;

use super::*;
use crate::auth::store::{FileCredentialStore, InMemoryCredentialStore};
use crate::auth::{AuthInteraction, CredentialInfo, LoginOptions, UpdateFn};

/// A provider that rotates the refresh token on the first exchange, like Anthropic does, and
/// answers `invalid_grant` for a superseded token. The response waits for `release`.
struct RotatingOAuth {
    valid_refresh: Mutex<String>,
    refreshes: AtomicUsize,
    rotated: Notify,
    release: Notify,
}

impl RotatingOAuth {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            valid_refresh: Mutex::new("old-refresh".into()),
            refreshes: AtomicUsize::new(0),
            rotated: Notify::new(),
            release: Notify::new(),
        })
    }

    fn refreshes(&self) -> usize {
        self.refreshes.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl OAuthAuth for RotatingOAuth {
    fn name(&self) -> &str {
        "Rotating OAuth"
    }

    async fn login(
        &self,
        _interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        _cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        Err(AuthError::Cancelled)
    }

    async fn refresh(&self, credential: &OAuthCredential, _cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        {
            let mut valid = self.valid_refresh.lock().unwrap();
            if *valid != credential.refresh {
                return Err(AuthError::other("400 invalid_grant"));
            }
            *valid = "new-refresh".into();
        }
        self.rotated.notify_one();
        self.release.notified().await;
        Ok(OAuthCredential {
            refresh: "new-refresh".into(),
            access: "new-access".into(),
            expires: now_ms() + 3_600_000,
            ..credential.clone()
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}

/// Signals `entered` when `update` is called, before it waits for the inner store's lock.
struct EnteredStore {
    inner: Arc<dyn CredentialStore>,
    entered: Notify,
}

#[async_trait]
impl CredentialStore for EnteredStore {
    async fn read(&self, provider_id: &str) -> AuthResult<Option<Credential>> {
        self.inner.read(provider_id).await
    }

    async fn list(&self) -> AuthResult<Vec<CredentialInfo>> {
        self.inner.list().await
    }

    async fn update(&self, provider_id: &str, transform: UpdateFn) -> AuthResult<Option<Credential>> {
        self.entered.notify_one();
        self.inner.update(provider_id, transform).await
    }

    async fn delete(&self, provider_id: &str) -> AuthResult<()> {
        self.inner.delete(provider_id).await
    }
}

fn expired() -> Credential {
    Credential::OAuth(OAuthCredential {
        refresh: "old-refresh".into(),
        access: "old-access".into(),
        expires: 0,
        ..Default::default()
    })
}

fn in_memory() -> Arc<dyn CredentialStore> {
    Arc::new(InMemoryCredentialStore::new([("acme".to_string(), expired())].into()))
}

fn resolver(store: &Arc<dyn CredentialStore>, oauth: &Arc<RotatingOAuth>) -> AuthResolver {
    let oauth = oauth.clone();
    AuthResolver::new(store.clone(), Arc::new(move |id| (id == "acme").then(|| oauth.clone() as Arc<dyn OAuthAuth>)))
}

async fn api_key(resolver: &AuthResolver) -> AuthResult<Option<String>> {
    Ok(resolver.resolve("acme", &AuthOverrides::default()).await?.and_then(|resolved| resolved.auth.api_key))
}

fn keep() -> UpdateFn {
    Box::new(|_: Option<Credential>| async { AuthResult::<Option<Credential>>::Ok(None) }.boxed())
}

/// Reads under the store lock, so it returns only after a running refresh has persisted or failed.
async fn locked_read(store: &Arc<dyn CredentialStore>) -> OAuthCredential {
    match store.update("acme", keep()).await.unwrap() {
        Some(Credential::OAuth(credential)) => credential,
        other => panic!("expected an OAuth credential, got {other:?}"),
    }
}

async fn rotated_token_survives_caller_abort(store: Arc<dyn CredentialStore>) {
    let oauth = RotatingOAuth::new();
    let first = Arc::new(resolver(&store, &oauth));
    let task = tokio::spawn({
        let first = first.clone();
        async move { api_key(&first).await }
    });
    oauth.rotated.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    oauth.release.notify_one();

    let saved = locked_read(&store).await;
    assert_eq!(
        (saved.refresh.as_str(), saved.access.as_str()),
        ("new-refresh", "new-access"),
        "the rotated refresh token was discarded with the aborted caller"
    );
    assert_eq!(api_key(&resolver(&store, &oauth)).await.unwrap().as_deref(), Some("new-access"));
    assert_eq!(oauth.refreshes(), 1);
}

#[tokio::test]
async fn rotated_token_survives_caller_abort_in_memory() {
    rotated_token_survives_caller_abort(in_memory()).await;
}

#[tokio::test]
async fn rotated_token_survives_caller_abort_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent").join("auth.json");
    let store: Arc<dyn CredentialStore> = Arc::new(FileCredentialStore::new(&path));
    let seed: UpdateFn = Box::new(|_: Option<Credential>| async { AuthResult::Ok(Some(expired())) }.boxed());
    store.update("acme", seed).await.unwrap();

    rotated_token_survives_caller_abort(store).await;
    let Some(Credential::OAuth(reopened)) = FileCredentialStore::new(&path).read("acme").await.unwrap() else {
        panic!("auth.json has no OAuth credential");
    };
    assert_eq!(reopened.refresh, "new-refresh");
}

#[tokio::test]
async fn caller_dropped_while_waiting_for_the_lock_starts_no_refresh() {
    let inner = in_memory();
    let entered = Arc::new(EnteredStore { inner: inner.clone(), entered: Notify::new() });
    let store: Arc<dyn CredentialStore> = entered.clone();
    let (held, gate) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let holder = tokio::spawn({
        let (inner, held, gate) = (inner.clone(), held.clone(), gate.clone());
        async move {
            let hold: UpdateFn = Box::new(move |_: Option<Credential>| {
                async move {
                    held.notify_one();
                    gate.notified().await;
                    AuthResult::Ok(None)
                }
                .boxed()
            });
            inner.update("acme", hold).await
        }
    });
    held.notified().await;

    let oauth = RotatingOAuth::new();
    oauth.release.notify_one();
    let caller = tokio::spawn({
        let (store, oauth) = (store.clone(), oauth.clone());
        async move { refresh_stored_oauth(&store, "acme", oauth, 0).await }
    });
    entered.entered.notified().await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    gate.notify_one();
    holder.await.unwrap().unwrap();

    assert_eq!(locked_read(&inner).await.refresh, "old-refresh");
    assert_eq!(oauth.refreshes(), 0);
}

#[tokio::test(start_paused = true)]
async fn refresh_times_out_and_keeps_the_stored_credential() {
    let store = in_memory();
    let oauth = RotatingOAuth::new();
    let error = refresh_stored_oauth(&store, "acme", oauth.clone(), 0).await.unwrap_err();
    assert!(error.to_string().contains("OAuth refresh failed for acme: timed out"), "{error}");
    assert_eq!(locked_read(&store).await.access, "old-access");
    assert_eq!(oauth.refreshes(), 1);
}
