//! Port of upstream `test/session-documents.test.ts`.

#[path = "../support/mod.rs"]
mod support;

mod lifecycle;
mod retirement;
mod transactions;

use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use pi_durable::documents::*;
use pi_durable::session::Session;
use pi_durable::*;
use serde::{Deserialize, Serialize};
use support::*;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Nested {
    pub count: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Label {
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Live {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub items: Vec<String>,
    pub nested: Nested,
    pub other: Label,
}

pub fn initial_live() -> Live {
    Live { message: None, items: vec![], nested: Nested::default(), other: Label { label: "x".into() } }
}

pub static LIVE_INIT_COUNT: AtomicUsize = AtomicUsize::new(0);

pub static LIVE_DOC: LazyLock<DocToken<Live, LatestConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new(
        "test.live",
        1,
        LatestConversation(LatestFork::Initial),
        || {
            LIVE_INIT_COUNT.fetch_add(1, Ordering::SeqCst);
            initial_live()
        },
        |_, _, _| Ok(false),
    ))
    .unwrap()
});

pub static REWINDABLE_LIVE_DOC: LazyLock<DocToken<Live, RewindableConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new("test.live", 1, RewindableConversation(DocumentFork::AsOf), initial_live, |_, _, _| {
        Ok(false)
    }))
    .unwrap()
});

pub static LIVE_DOC_V2: LazyLock<DocToken<Live, LatestConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new("test.live", 2, LatestConversation(LatestFork::Initial), initial_live, |_, _, _| Ok(false)))
        .unwrap()
});

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Counter {
    pub count: f64,
}

pub static COUNTER_DOC: LazyLock<DocToken<Counter, SessionScope>> = LazyLock::new(|| {
    define_doc(DocSpec::new("test.counter", 1, SessionScope, || Counter { count: 0.0 }, |_, _, _| Ok(false))).unwrap()
});

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub seed: String,
    pub hits: f64,
}

pub static MEMBER_DOC: LazyLock<DocFamilyToken<Member, String, SessionScope>> = LazyLock::new(|| {
    define_doc_family(DocFamilySpec::new(
        "test.member",
        1,
        SessionScope,
        |seed: String| Member { seed, hits: 0.0 },
        |_, _, _| Ok(false),
    ))
    .unwrap()
});

/// A Session with one conversation whose live document holds items `a` and `b`.
pub async fn setup_live() -> (TestSession, ConversationId) {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    test.session
        .commit(
            move |tx| async move {
                let live = tx.doc(&LIVE_DOC, conversation_id).await?;
                live.edit(|live| live.items.extend(["a".to_owned(), "b".to_owned()]))
            },
            &context(),
        )
        .await
        .unwrap();
    (test, conversation_id)
}

pub async fn live_snapshot(session: &Session, conversation_id: ConversationId) -> Option<pi_chord::Json> {
    session.snapshot(&LIVE_DOC, conversation_id, &context()).await.unwrap()
}

pub async fn set_message(session: &Session, conversation_id: ConversationId, message: &str) {
    let message = message.to_owned();
    session
        .commit(
            move |tx| async move { tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| live.message = Some(message)) },
            &context(),
        )
        .await
        .unwrap();
}
