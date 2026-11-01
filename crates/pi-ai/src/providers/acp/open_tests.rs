//! `open_in` on a slot that `close_session` removed while the open waited: it fails before it
//! starts an agent, and the current slot still starts one.

use std::sync::Arc;

use super::*;

fn sentinel() -> AgentLaunch {
    AgentLaunch {
        provider: "sentinel-acp".into(),
        name: "Sentinel".into(),
        command: "/nonexistent/sentinel-acp-agent".into(),
        ..AgentLaunch::default()
    }
}

#[tokio::test]
async fn an_open_on_a_closed_slot_starts_no_agent() {
    let agent = sentinel();
    let old = slot(&agent.provider, "closed-pi");
    close_session("closed-pi").await;
    let error = open_in(&agent, "closed-pi", old.clone()).await.unwrap_err();
    assert!(error.contains("was closed"), "{error}");
    assert!(old.lock().await.is_none(), "the removed slot holds no runtime");

    let current = slot(&agent.provider, "closed-pi");
    assert!(!Arc::ptr_eq(&old, &current), "a later open gets a new slot");
    let started = open_in(&agent, "closed-pi", current.clone()).await.unwrap_err();
    assert!(started.contains("Cannot start Sentinel"), "the current slot tries the start: {started}");
    assert!(current.lock().await.is_none());
    close_session("closed-pi").await;
}
