//! The `Transcript` service: the root conversation's durable view (upstream `services/transcript.ts` and
//! `transcript-provider.ts`).

use std::sync::{Arc, LazyLock};

use pi_chord::facets::{Facet, define_facet};
use pi_chord::services::{ServiceImpl, ServiceRef};
use pi_chord::{ChordResult, Context};
use pi_durable::harness::Conversation;

/// Holds one `state` member: active entries and the live, inbox, agent, and usage documents.
pub static TRANSCRIPT: LazyLock<ServiceRef> = LazyLock::new(|| ServiceRef::remote("pi.transcript"));

/// Serve the conversation's durable view state. The facet owns and disposes the attached state.
pub async fn create_transcript_service_facet(
    conversation: &Conversation,
    context: &Context,
) -> ChordResult<Arc<dyn Facet>> {
    let state = conversation.view_state(context).await?;
    Ok(define_facet("@pi/transcript", move |env| {
        let owned = state.clone();
        env.own(move || owned.dispose())?;
        env.provide(&TRANSCRIPT, ServiceImpl::new().state("state", &state))
    }))
}
