//! Server Session directory and management services (upstream `services/sessions.ts`).

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use pi_chord::services::{ServiceHandle, ServiceImpl, ServiceRef, StateHandle};
use pi_chord::{ChordResult, Context, Json};
use serde::{Deserialize, Serialize};

use super::codec::{decode_arg, decode_result, encode};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub server_id: String,
    pub session_id: String,
    pub created_at: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionCreateOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionDirectoryState {
    pub revision: u64,
    pub sessions: Vec<SessionSummary>,
}

/// `pi.session-directory`: one `state` member holding [`SessionDirectoryState`].
pub static SESSION_DIRECTORY: LazyLock<ServiceRef> = LazyLock::new(|| ServiceRef::remote("pi.session-directory"));
pub static SESSION_MANAGEMENT: LazyLock<ServiceRef> = LazyLock::new(|| ServiceRef::remote("pi.session-management"));

/// The directory state from a consumer handle, once hydrated.
pub fn session_directory_state(handle: &ServiceHandle) -> ChordResult<Option<SessionDirectoryState>> {
    let state: StateHandle = handle.state("state")?;
    state.value()?.map(|value| decode_result(Some(value))).transpose()
}

#[async_trait]
pub trait SessionManagement: Send + Sync {
    async fn create(&self, options: SessionCreateOptions, context: &Context) -> ChordResult<SessionSummary>;
    async fn remove(&self, session_id: String, context: &Context) -> ChordResult<()>;
    async fn attach(&self, session_id: String, context: &Context) -> ChordResult<()>;
    async fn detach(&self, context: &Context) -> ChordResult<()>;
}

pub fn session_management_impl(service: Arc<dyn SessionManagement>) -> ServiceImpl {
    let (create, remove, attach, detach) = (service.clone(), service.clone(), service.clone(), service);
    ServiceImpl::new()
        .method("create", move |args: Vec<Json>, context| {
            let service = create.clone();
            async move { encode(&service.create(decode_arg(&args, 0)?, &context).await?).map(Some) }
        })
        .method("remove", move |args: Vec<Json>, context| {
            let service = remove.clone();
            async move { service.remove(decode_arg(&args, 0)?, &context).await.map(|()| None) }
        })
        .method("attach", move |args: Vec<Json>, context| {
            let service = attach.clone();
            async move { service.attach(decode_arg(&args, 0)?, &context).await.map(|()| None) }
        })
        .method("detach", move |_args: Vec<Json>, context| {
            let service = detach.clone();
            async move { service.detach(&context).await.map(|()| None) }
        })
}

/// Calls `pi.session-management` through a service handle.
#[derive(Clone)]
pub struct SessionManagementClient(pub ServiceHandle);

#[async_trait]
impl SessionManagement for SessionManagementClient {
    async fn create(&self, options: SessionCreateOptions, context: &Context) -> ChordResult<SessionSummary> {
        decode_result(self.0.call("create", vec![encode(&options)?], context).await?)
    }

    async fn remove(&self, session_id: String, context: &Context) -> ChordResult<()> {
        self.0.call("remove", vec![Json::from(session_id)], context).await.map(drop)
    }

    async fn attach(&self, session_id: String, context: &Context) -> ChordResult<()> {
        self.0.call("attach", vec![Json::from(session_id)], context).await.map(drop)
    }

    async fn detach(&self, context: &Context) -> ChordResult<()> {
        self.0.call("detach", Vec::new(), context).await.map(drop)
    }
}
