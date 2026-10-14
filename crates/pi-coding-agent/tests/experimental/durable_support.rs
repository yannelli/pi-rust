//! Port of upstream `test/experimental-durable-support.ts`.

use std::sync::Arc;

use pi_chord::BACKGROUND_CONTEXT;
use pi_durable::harness::types::{AgentChange, ModelRef};
use pi_durable::harness::{Conversation, CreateOptions, Harness, HarnessOptions, RegistryReader, create_registry};
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{FauxResponseStep, RegisterFauxProviderOptions, faux_provider};
use tokio::sync::Notify;

pub struct FauxConversation {
    pub harness: Harness,
    pub conversation: Conversation,
}

impl FauxConversation {
    pub async fn close(&self) {
        self.harness.close(&BACKGROUND_CONTEXT).await.unwrap();
    }
}

/// An in-memory durable Harness whose root conversation answers with the faux provider.
pub async fn open_faux_conversation(responses: Vec<FauxResponseStep>) -> FauxConversation {
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses(responses);
    let registry: Arc<dyn RegistryReader> = Arc::new(create_registry());
    let options = HarnessOptions::new(faux.provider(), registry);
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &BACKGROUND_CONTEXT).await.unwrap();
    let model = faux.get_model();
    let agent = AgentChange::model(ModelRef { provider: model.provider.clone(), model_id: model.id.clone() });
    let conversation = harness.root(&BACKGROUND_CONTEXT, CreateOptions::default().with_agent(agent)).await.unwrap();
    FauxConversation { harness, conversation }
}

/// A faux response that never arrives; it ends only when its generation is aborted.
pub fn pending_response() -> (FauxResponseStep, Arc<Notify>) {
    let reached = Arc::new(Notify::new());
    let notify = reached.clone();
    let step = FauxResponseStep::async_factory(move |_, options, _, _| {
        let (notify, signal) = (notify.clone(), options.signal.clone());
        Box::pin(async move {
            notify.notify_one();
            match signal {
                Some(signal) => Err(signal.cancelled().await.to_string()),
                None => std::future::pending().await,
            }
        })
    });
    (step, reached)
}
