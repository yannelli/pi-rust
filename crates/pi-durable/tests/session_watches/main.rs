//! Port of upstream `test/session-watches.test.ts`.

#[path = "../support/mod.rs"]
mod support;

mod delivery;
mod lifetime;

use std::sync::{Arc, LazyLock, Mutex};

use pi_chord::Json;
use pi_chord::delta::Op;
use pi_durable::documents::*;
use pi_durable::session::{CommittedWatch, Session};
use support::*;

pub static STATE_DOC: LazyLock<DocToken<Json, SessionScope>> = LazyLock::new(|| {
    let initial = || json(r#"{"value":0,"items":["a","b"],"retained":{"label":"stable"}}"#);
    define_doc(DocSpec::new("watch.state", 1, SessionScope, initial)).unwrap()
});

pub async fn create_state() -> TestSession {
    let test = open_test_session();
    test.session.commit(|tx| async move { tx.doc(&STATE_DOC, ()).await.map(drop) }, &context()).await.unwrap();
    test
}

pub async fn set_value(session: &Session, value: i64) {
    session
        .commit(
            move |tx| async move {
                tx.doc(&STATE_DOC, ()).await?.update(|state| state.set("value", value))?.map_err(Into::into)
            },
            &context(),
        )
        .await
        .map_err(|error| error.to_string())
        .unwrap();
}

pub async fn retire_state(session: &Session) {
    session.commit(|tx| async move { tx.retire_doc(&STATE_DOC, ()).await }, &context()).await.unwrap();
}

pub async fn watch_state(session: &Session) -> CommittedWatch {
    session.watch_doc(&STATE_DOC, (), &context()).await.unwrap().unwrap()
}

/// The `value` field of a delivered revision, or -1 once retired.
pub fn value_of(value: &Json) -> i64 {
    value.get("value").and_then(Json::as_i64).unwrap_or(-1)
}

pub type Frames = Arc<Mutex<Vec<(Json, Vec<Op>)>>>;

/// Start `watch` with a listener that records every frame.
pub fn record(watch: &CommittedWatch) -> Frames {
    let frames: Frames = Arc::default();
    let sink = frames.clone();
    watch
        .start(move |value, ops, _| {
            sink.lock().unwrap().push((value, ops));
            async { Ok(()) }
        })
        .unwrap();
    frames
}
