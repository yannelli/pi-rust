//! `/llama` for the interactive mode (upstream built-in extension `extensions/llama/index.ts`):
//! thin wrappers over [`LlamaManager`] and the router client that keep the llama.cpp models of
//! the model registry in step with the router.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::AgentSession;
use crate::llama::{
    HuggingFaceClient, HuggingFaceModel, HuggingFaceModelDetails, LLAMA_PROVIDER_ID, LlamaCatalogSync, LlamaClient,
    LlamaError, LlamaManager, LlamaModelInfo, LlamaProgress, LlamaProvider, LlamaUi, configured_llama_client,
    find_hugging_face_token,
};

/// Progress of a load or download, called from the request task.
pub type LlamaProgressFn<'a> = &'a (dyn Fn(LlamaProgress) + Send + Sync);

impl AgentSession {
    /// The llama.cpp provider; `None` when `builtin:llama.cpp` is off.
    pub fn llama_provider(&self) -> Option<Arc<LlamaProvider>> {
        self.model_runtime().llama_provider()
    }

    /// A client for the configured router. The error is the notice to show, such as
    /// "Configure llama.cpp with /login llama.cpp".
    pub async fn llama_client(&self) -> Result<LlamaClient, String> {
        if self.llama_provider().is_none() {
            return Err("The llama.cpp extension is disabled (builtin:llama.cpp)".into());
        }
        let resolved = match self.model_runtime().resolve_provider_auth(LLAMA_PROVIDER_ID).await {
            Some(Ok(resolved)) => resolved,
            Some(Err(error)) => return Err(error),
            None => None,
        };
        configured_llama_client(resolved.as_ref())
    }

    async fn llama_parts(&self) -> Result<(LlamaClient, Arc<LlamaProvider>), LlamaError> {
        let client = self.llama_client().await.map_err(LlamaError::Server)?;
        let provider = self.llama_provider().ok_or_else(|| LlamaError::Server("llama.cpp is disabled".into()))?;
        Ok((client, provider))
    }

    /// Hugging Face search with the token from `HF_TOKEN` or the Hugging Face CLI login.
    pub fn hugging_face_client(&self) -> HuggingFaceClient {
        HuggingFaceClient::new(find_hugging_face_token(), None)
    }

    /// Run the whole `/llama` flow (model list, load, unload, download) on `ui`.
    pub async fn run_llama(&self, ui: &dyn LlamaUi) -> Result<(), String> {
        let client = self.llama_client().await?;
        let provider = self.llama_provider().ok_or("llama.cpp is disabled")?;
        let runtime = self.model_runtime().clone();
        let manager = LlamaManager {
            client,
            provider: &provider,
            ui,
            sync: runtime.as_ref(),
            hugging_face: self.hugging_face_client(),
        };
        manager.run().await;
        Ok(())
    }

    /// Show `catalog` (or the router's current list) in `/model` and the registry.
    async fn sync_llama_catalog(
        &self,
        client: &LlamaClient,
        provider: &LlamaProvider,
        catalog: Option<Vec<LlamaModelInfo>>,
    ) -> Result<Vec<LlamaModelInfo>, LlamaError> {
        let catalog = match catalog {
            Some(catalog) => catalog,
            None => client.list(false, &CancellationToken::new()).await?,
        };
        provider.set_catalog(&catalog, client.server_url(), false);
        self.model_runtime().refresh_llama_models().await.map_err(LlamaError::Server)?;
        Ok(catalog)
    }

    /// The router's models; loaded and sleeping ones become selectable chat models.
    pub async fn llama_list_models(&self) -> Result<Vec<LlamaModelInfo>, LlamaError> {
        let (client, provider) = self.llama_parts().await?;
        self.sync_llama_catalog(&client, &provider, None).await
    }

    /// Load `model` and wait until it is ready; `cancel` stops waiting (the caller unloads).
    pub async fn llama_load_model(
        &self,
        model: &str,
        progress: LlamaProgressFn<'_>,
        cancel: &CancellationToken,
    ) -> Result<LlamaModelInfo, LlamaError> {
        let (client, provider) = self.llama_parts().await?;
        let loaded = client.load_and_wait(model, progress, cancel).await?;
        self.sync_llama_catalog(&client, &provider, None).await?;
        Ok(loaded)
    }

    /// Unload `model` and wait until the router reports it unloaded.
    pub async fn llama_unload_model(&self, model: &str, cancel: &CancellationToken) -> Result<(), LlamaError> {
        let (client, provider) = self.llama_parts().await?;
        client.unload_and_wait(model, cancel).await?;
        self.sync_llama_catalog(&client, &provider, None).await.map(drop)
    }

    /// Search Hugging Face for GGUF repositories.
    pub async fn llama_search(
        &self,
        query: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<HuggingFaceModel>, LlamaError> {
        self.hugging_face_client().search(query, cancel).await
    }

    /// Gated access and quantizations of a Hugging Face repository.
    pub async fn llama_model_details(
        &self,
        repository: &str,
        cancel: &CancellationToken,
    ) -> Result<HuggingFaceModelDetails, LlamaError> {
        self.hugging_face_client().details(repository, cancel).await
    }

    /// Download `model` (`owner/repository[:quant]`) through the router, reporting bytes.
    pub async fn llama_download_model(
        &self,
        model: &str,
        progress: LlamaProgressFn<'_>,
        cancel: &CancellationToken,
    ) -> Result<Vec<LlamaModelInfo>, LlamaError> {
        let (client, provider) = self.llama_parts().await?;
        let catalog = client.download_and_wait(model, progress, cancel).await?;
        self.sync_llama_catalog(&client, &provider, Some(catalog)).await
    }

    /// Stop a load or download: the router unloads the model.
    pub async fn llama_cancel(&self, model: &str) -> Result<(), LlamaError> {
        let (client, provider) = self.llama_parts().await?;
        client.unload(model, &CancellationToken::new()).await?;
        self.sync_llama_catalog(&client, &provider, None).await.map(drop)
    }
}
