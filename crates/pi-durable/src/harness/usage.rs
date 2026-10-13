//! The `pi.provider` and `pi.usage` documents (upstream `harness/provider.ts` and `harness/usage.ts`).

use std::sync::{Arc, LazyLock, Mutex};

use pi_ai::Usage;
use pi_chord::{Context, JsonMap};
use serde::{Deserialize, Serialize};

use super::scheduler::TaskRuntime;
use crate::documents::{DocSpec, DocToken, LatestConversation, LatestFork, define_doc, encode};
use crate::errors::{DurableError, DurableResult};
use crate::ids::ConversationId;
use crate::session::Tx;

/// Stable provider-facing identity of one conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderState {
    pub session_id: String,
}

/// Built-in provider state; every fork starts with a fresh identity instead of copying its parent.
pub static PROVIDER_DOC: LazyLock<DocToken<ProviderState, LatestConversation>> = LazyLock::new(|| {
    let spec = DocSpec::new("pi.provider", 1, LatestConversation(LatestFork::Initial), || ProviderState {
        session_id: crate::util::uuidv7(),
    })
    .checkpoint_when(|_, _, _| Ok(true));
    define_doc(spec).unwrap_or_else(|_| unreachable!("pi.provider is a valid definition"))
});

/// The persisted identity, without a write in the normal path. A conversation without `pi.provider` gets one
/// migration commit before the provider request starts.
pub async fn ensure_provider_session_id(runtime: &TaskRuntime, context: &Context) -> DurableResult<String> {
    let conversation_id = runtime.conversation_id();
    if let Some(existing) = runtime.snapshot(&PROVIDER_DOC, conversation_id, context).await? {
        return Ok(PROVIDER_DOC.decode(&existing)?.session_id);
    }
    let created = Arc::new(Mutex::new(None));
    let slot = created.clone();
    runtime
        .commit(
            move |tx, _| async move {
                let state = tx.doc(&PROVIDER_DOC, conversation_id).await?.get()?;
                *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(state.session_id);
                Ok(None)
            },
            context,
        )
        .await?;
    let created = created.lock().unwrap_or_else(|p| p.into_inner()).take();
    created.ok_or_else(|| DurableError::error(format!("Conversation {conversation_id} has no provider session ID")))
}

/// Ledger of one conversation's own spend: its entries, and compaction summarization requests.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageState {
    /// Assistant entries and summarization requests, keyed `provider/modelId`.
    pub models: JsonMap,
    /// Tool results, keyed by tool name.
    pub tools: JsonMap,
}

/// Which bucket of `pi.usage` a spend goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageBucket {
    Models,
    Tools,
}

pub static USAGE_DOC: LazyLock<DocToken<UsageState, LatestConversation>> = LazyLock::new(|| {
    let spec = DocSpec::new("pi.usage", 1, LatestConversation(LatestFork::Initial), UsageState::default)
        .checkpoint_when(|_, _, _| Ok(true));
    define_doc(spec).unwrap_or_else(|_| unreachable!("pi.usage is a valid definition"))
});

/// Add `usage` to one bucket of the conversation's `pi.usage`, in the commit that records the response.
pub async fn record_usage(
    tx: &Tx,
    conversation_id: ConversationId,
    bucket: UsageBucket,
    key: &str,
    usage: &Usage,
) -> DurableResult<()> {
    let draft = tx.doc(&USAGE_DOC, conversation_id).await?;
    let encoded = encode(usage)?;
    draft.edit(|state| {
        let totals = match bucket {
            UsageBucket::Models => &mut state.models,
            UsageBucket::Tools => &mut state.tools,
        };
        match totals.get(key).and_then(|total| total.deserialize_into::<Usage>().ok()) {
            Some(mut total) => {
                add_usage(&mut total, usage);
                if let Ok(value) = encode(&total) {
                    totals.insert(key, value);
                }
            }
            None => {
                totals.insert(key, encoded);
            }
        }
    })
}

/// Add every counter of `usage` to `total`; optional counters are added once either side reports them.
pub fn add_usage(total: &mut Usage, usage: &Usage) {
    total.input += usage.input;
    total.output += usage.output;
    total.cache_read += usage.cache_read;
    total.cache_write += usage.cache_write;
    total.total_tokens += usage.total_tokens;
    if let Some(value) = usage.cache_write_1h {
        total.cache_write_1h = Some(total.cache_write_1h.unwrap_or(0) + value);
    }
    if let Some(value) = usage.reasoning {
        total.reasoning = Some(total.reasoning.unwrap_or(0) + value);
    }
    total.cost.input += usage.cost.input;
    total.cost.output += usage.cost.output;
    total.cost.cache_read += usage.cost.cache_read;
    total.cost.cache_write += usage.cost.cache_write;
    total.cost.total += usage.cost.total;
}

/// Add every bucket of `state` into `sum`.
pub fn add_usage_state(sum: &mut UsageState, state: &UsageState) -> DurableResult<()> {
    for (target, source) in [(&mut sum.models, &state.models), (&mut sum.tools, &state.tools)] {
        for (key, usage) in source.iter() {
            let usage: Usage = usage.deserialize_into()?;
            let next = match target.get(key) {
                Some(total) => {
                    let mut total: Usage = total.deserialize_into()?;
                    add_usage(&mut total, &usage);
                    total
                }
                None => usage,
            };
            target.insert(key.clone(), encode(&next)?);
        }
    }
    Ok(())
}

/// A bucket entry decoded as `Usage`.
pub fn usage_of(totals: &JsonMap, key: &str) -> Option<Usage> {
    totals.get(key).and_then(|total| total.deserialize_into().ok())
}
