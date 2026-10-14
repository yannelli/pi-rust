//! Port of upstream `test/experimental-session-worker-lifecycle.test.ts`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use pi_coding_agent::experimental::session_worker::{WorkerLifecycle, WorkerLifecycleOptions};

const GENERATION: &str = "generation-1";

fn create_lifecycle() -> (WorkerLifecycle, Arc<AtomicUsize>) {
    let retired = Arc::new(AtomicUsize::new(0));
    let counter = retired.clone();
    let lifecycle = WorkerLifecycle::new(WorkerLifecycleOptions {
        initial_server_connection_id: Some(GENERATION.into()),
        initial_demand_grace: Duration::from_millis(100),
        orphan_demand_grace: Duration::from_millis(200),
        on_retire: Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
    });
    (lifecycle, retired)
}

async fn advance(ms: u64) {
    tokio::time::advance(Duration::from_millis(ms)).await;
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
}

fn count(retired: &AtomicUsize) -> usize {
    retired.load(Ordering::SeqCst)
}

#[tokio::test(start_paused = true)]
async fn retires_only_after_client_demand_and_harness_activity_are_both_gone() {
    let (lifecycle, retired) = create_lifecycle();
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    lifecycle.set_harness_active(true);
    lifecycle.set_demand(GENERATION, "attachment-1", false).unwrap();
    assert_eq!(count(&retired), 0);
    lifecycle.set_harness_active(false);
    assert_eq!(count(&retired), 1);
    lifecycle.close();
}

#[tokio::test(start_paused = true)]
async fn retains_the_worker_until_every_presentation_attachment_is_released() {
    let (lifecycle, retired) = create_lifecycle();
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    lifecycle.set_demand(GENERATION, "attachment-2", true).unwrap();
    lifecycle.set_demand(GENERATION, "attachment-1", false).unwrap();
    assert_eq!(count(&retired), 0);
    lifecycle.set_demand(GENERATION, "attachment-2", false).unwrap();
    assert_eq!(count(&retired), 1);
    lifecycle.close();
}

#[tokio::test(start_paused = true)]
async fn does_not_retire_on_repeated_activity_updates_while_the_harness_stays_active() {
    let (lifecycle, retired) = create_lifecycle();
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    lifecycle.set_harness_active(true);
    lifecycle.set_demand(GENERATION, "attachment-1", false).unwrap();
    lifecycle.set_harness_active(true);
    assert_eq!(count(&retired), 0);
    lifecycle.set_harness_active(false);
    assert_eq!(count(&retired), 1);
    lifecycle.close();
}

#[tokio::test(start_paused = true)]
async fn an_idle_harness_update_does_not_retire_while_demand_remains() {
    let (lifecycle, retired) = create_lifecycle();
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    lifecycle.set_harness_active(false);
    assert_eq!(count(&retired), 0);
    lifecycle.close();
}

#[tokio::test(start_paused = true)]
async fn does_not_retire_while_a_demand_acknowledgement_holds_reconciliation() {
    let (lifecycle, retired) = create_lifecycle();
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    let mut hold = lifecycle.hold_retirement();
    lifecycle.set_demand(GENERATION, "attachment-1", false).unwrap();
    assert_eq!(count(&retired), 0);
    hold.release();
    assert_eq!(count(&retired), 1);
    lifecycle.close();
}

#[tokio::test(start_paused = true)]
async fn holds_retirement_only_for_requests_from_the_active_attachment() {
    let (lifecycle, retired) = create_lifecycle();
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    let mut hold = lifecycle.begin_request(GENERATION, "attachment-1").unwrap();
    lifecycle.set_demand(GENERATION, "attachment-1", false).unwrap();
    assert_eq!(count(&retired), 0);
    hold.release();
    assert_eq!(count(&retired), 1);
    let error = lifecycle.begin_request(GENERATION, "attachment-1").err().unwrap();
    assert!(error.to_string().contains("retiring"));
    lifecycle.close();
}

#[tokio::test(start_paused = true)]
async fn rejects_requests_from_stale_generations_and_attachments() {
    let (lifecycle, _) = create_lifecycle();
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    let stale = lifecycle.begin_request("stale", "attachment-1").err().unwrap();
    assert!(stale.to_string().contains("stale server generation"));
    let wrong = lifecycle.begin_request(GENERATION, "wrong-attachment").err().unwrap();
    assert!(wrong.to_string().contains("active attachment"));
    lifecycle.close();
}

#[tokio::test(start_paused = true)]
async fn retains_disconnected_generation_demand_for_the_orphan_grace() {
    let (lifecycle, retired) = create_lifecycle();
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    lifecycle.server_disconnected(GENERATION);
    advance(199).await;
    assert_eq!(count(&retired), 0);
    advance(1).await;
    assert_eq!(count(&retired), 1);
    lifecycle.close();
}

#[tokio::test(start_paused = true)]
async fn allows_a_replacement_generation_to_retain_the_worker() {
    let (lifecycle, retired) = create_lifecycle();
    lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap();
    lifecycle.server_disconnected(GENERATION);
    lifecycle.server_connected("generation-2");
    lifecycle.set_demand("generation-2", "attachment-2", true).unwrap();
    advance(200).await;
    assert_eq!(count(&retired), 0);
    lifecycle.set_demand("generation-2", "attachment-2", false).unwrap();
    assert_eq!(count(&retired), 1);
    lifecycle.close();
}

#[tokio::test(start_paused = true)]
async fn retires_a_launched_worker_that_never_receives_initial_demand() {
    let (lifecycle, retired) = create_lifecycle();
    advance(100).await;
    assert_eq!(count(&retired), 1);
    lifecycle.close();
}

#[tokio::test(start_paused = true)]
async fn rejects_demand_after_retirement_has_won_the_race() {
    let (lifecycle, _) = create_lifecycle();
    advance(100).await;
    let error = lifecycle.set_demand(GENERATION, "attachment-1", true).unwrap_err();
    assert!(error.to_string().contains("retiring"));
    lifecycle.close();
}

#[tokio::test(start_paused = true)]
async fn rejects_demand_from_a_stale_server_generation() {
    let (lifecycle, _) = create_lifecycle();
    let error = lifecycle.set_demand("stale", "attachment-1", true).unwrap_err();
    assert!(error.to_string().contains("stale server generation"));
    lifecycle.close();
}
