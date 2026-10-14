//! Plugin services (upstream `services/plugins.ts`).

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use pi_chord::services::{ServiceHandle, ServiceImpl, ServiceRef};
use pi_chord::{ChordResult, Context, Json};
use serde::{Deserialize, Serialize};

use super::codec::decode_arg;

/// Server-built plugin generations available to presentations.
pub static PRESENTATION_PLUGINS: LazyLock<ServiceRef> = LazyLock::new(|| ServiceRef::remote("pi.presentation-plugins"));
/// Plugin facets hosted in the currently attached Session worker.
pub static SESSION_PLUGINS: LazyLock<ServiceRef> = LazyLock::new(|| ServiceRef::remote("pi.session-plugins"));

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareSessionRequest {
    pub session_id: String,
    pub package_paths: Option<Vec<String>>,
}

#[async_trait]
pub trait PresentationPlugins: Send + Sync {
    async fn prepare_session(&self, request: PrepareSessionRequest, context: &Context) -> ChordResult<Json>;
    async fn reload(&self, context: &Context) -> ChordResult<Json>;
}

#[async_trait]
pub trait SessionPlugins: Send + Sync {
    async fn reload(&self, context: &Context) -> ChordResult<()>;
}

pub fn presentation_plugins_impl(service: Arc<dyn PresentationPlugins>) -> ServiceImpl {
    let prepare = service.clone();
    ServiceImpl::new()
        .method("prepareSession", move |args: Vec<Json>, context| {
            let service = prepare.clone();
            async move { service.prepare_session(decode_arg(&args, 0)?, &context).await.map(Some) }
        })
        .method("reload", move |_args: Vec<Json>, context| {
            let service = service.clone();
            async move { service.reload(&context).await.map(Some) }
        })
}

pub fn session_plugins_impl(service: Arc<dyn SessionPlugins>) -> ServiceImpl {
    ServiceImpl::new().method("reload", move |_args: Vec<Json>, context| {
        let service = service.clone();
        async move { service.reload(&context).await.map(|()| None) }
    })
}

#[derive(Clone)]
pub struct PresentationPluginsClient(pub ServiceHandle);

#[async_trait]
impl PresentationPlugins for PresentationPluginsClient {
    async fn prepare_session(&self, request: PrepareSessionRequest, context: &Context) -> ChordResult<Json> {
        let result = self.0.call("prepareSession", vec![super::encode(&request)?], context).await?;
        Ok(result.unwrap_or(Json::Null))
    }

    async fn reload(&self, context: &Context) -> ChordResult<Json> {
        Ok(self.0.call("reload", Vec::new(), context).await?.unwrap_or(Json::Null))
    }
}

#[derive(Clone)]
pub struct SessionPluginsClient(pub ServiceHandle);

#[async_trait]
impl SessionPlugins for SessionPluginsClient {
    async fn reload(&self, context: &Context) -> ChordResult<()> {
        self.0.call("reload", Vec::new(), context).await.map(drop)
    }
}
