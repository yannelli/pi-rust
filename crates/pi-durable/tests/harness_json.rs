//! Port of upstream `test/harness-json.test.ts`. A partial commit writes its value leaf by leaf (`DraftCore::set`).
//! Upstream guards `assignJson()` against merging a new `__proto__` member into `Object.prototype`; a Rust JSON map
//! has no prototype, so the member is an own member like any other.

#[path = "support/mod.rs"]
mod support;

use std::sync::LazyLock;

use pi_chord::Json;
use pi_durable::documents::*;
use pi_durable::session::{Session, Tx};
use support::*;

static DOC: LazyLock<DocToken<Json, SessionScope>> = LazyLock::new(|| {
    define_doc(DocSpec::new("json.assign", 1, SessionScope, || json("{}"), |_, _, _| Ok(false))).unwrap()
});

static OTHER: LazyLock<DocToken<Json, SessionScope>> = LazyLock::new(|| {
    define_doc(DocSpec::new("json.other", 1, SessionScope, || json("{}"), |_, _, _| Ok(false))).unwrap()
});

async fn assign(session: &Session, message: &str) {
    let value = json(&format!(r#"{{"message":{message}}}"#));
    let change = move |tx: Tx| async move { tx.doc(&DOC, ()).await?.set(&value) };
    session.commit(change, &context()).await.unwrap();
}

#[tokio::test]
async fn does_not_merge_a_new_proto_member_into_anything_else() {
    let test = open_test_session();
    assign(&test.session, r#"{"args":{"path":"x"}}"#).await;
    assign(&test.session, r#"{"args":{"path":"xy","__proto__":{"polluted":true}}}"#).await;
    let value = test.session.snapshot(&DOC, (), &context()).await.unwrap().unwrap();
    let args = &value["message"]["args"];
    assert_eq!(args.get("__proto__"), Some(&json(r#"{"polluted":true}"#)));
    assert_eq!(args.get("path"), Some(&Json::from("xy")));
    // `__proto__` is not a safe op path, so the change sets the object that holds it whole.
    let ops = r#"[["s",["message","args"],{"path":"xy","__proto__":{"polluted":true}}]]"#;
    let ops = pi_chord::delta::parse_ops(&json(ops)).unwrap();
    let pi_durable::DocumentCommitChange::Document { ops: published, .. } =
        &document_changes(&test.last_publication())[0]
    else {
        panic!("a document change")
    };
    assert_eq!(*published, ops);
    // Nothing else gained the member.
    let fresh = test.session.commit(|tx| async move { tx.doc(&OTHER, ()).await?.get() }, &context()).await.unwrap();
    assert_eq!(fresh, json("{}"));
    assert!(value.get("polluted").is_none() && value["message"].get("polluted").is_none());
}
