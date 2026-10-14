//! Narrow process-local UI capabilities for presentation facets (upstream `services/presentation-ui.ts`).

use std::sync::LazyLock;

use async_trait::async_trait;
use pi_chord::services::{Service, define_local_service};
use pi_chord::{ChordResult, Context};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentationSelectItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

#[async_trait]
pub trait PresentationUi: Send + Sync {
    async fn select(
        &self,
        title: &str,
        items: Vec<PresentationSelectItem>,
        selected_value: Option<String>,
        context: &Context,
    ) -> ChordResult<Option<String>>;
    fn show_status(&self, message: &str, context: &Context);
}

pub static PRESENTATION_UI: LazyLock<Service<dyn PresentationUi>> =
    LazyLock::new(|| define_local_service("pi.local.presentation-ui").expect("valid service ID"));
