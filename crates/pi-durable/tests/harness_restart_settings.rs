//! Upstream `harness-restart-failure.test.ts`, "the restart limit on the normal path": only a task that was cut off
//! reads `restart_limit`. Its own target with public API both versions share, so it also runs on the old source.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use support::chat::{add_tool, answer, call, no_parameters};
use support::harness_support::{chat_setup, ctx, open_chat_with};

// Measured on c27a914 with this test: 199 settings reads per round.
const BASELINE_SETTINGS_READS_PER_ROUND: usize = 199;

#[tokio::test]
async fn resolves_the_settings_no_more_often_than_before_only_a_task_that_was_cut_off_reads_the_limit() {
    let setup = chat_setup(Default::default());
    let resolutions = Arc::new(AtomicUsize::new(0));
    let counted = resolutions.clone();
    let mut options = setup.options();
    options.settings = Some(Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        Ok(HarnessSettings::default())
    }));
    let tool = define_tool("t", "t", no_parameters(), |_, _, _| async { Ok(ToolExecutionResult::default()) });
    add_tool(&setup, tool.with_replay(ToolReplay::Safe));
    let rounds = 5;
    let responses = (0..=rounds).flat_map(|round| [call("t", &format!("c{round}")), answer("done")]);
    setup.faux.set_responses(responses.collect::<Vec<_>>());
    let (harness, root) = open_chat_with(Arc::new(MemoryStorage::new()), &setup, options).await;
    root.submit(SubmissionDraft::input("warm up"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let mut per_round = Vec::new();
    for round in 0..rounds {
        let before = resolutions.load(Ordering::SeqCst);
        let input = SubmissionDraft::input(format!("round {round}"));
        root.submit(input, &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
        per_round.push(resolutions.load(Ordering::SeqCst) - before);
    }
    // Printed so the count can be compared with the source before the restart-failure port.
    eprintln!("settings resolutions per round: {per_round:?}");
    // One generation round with one tool call costs the same every time.
    assert!(per_round.iter().all(|count| *count == per_round[0] && *count > 0), "{per_round:?}");
    assert!(per_round.iter().all(|count| *count <= BASELINE_SETTINGS_READS_PER_ROUND), "{per_round:?}");
    harness.close(&ctx()).await.unwrap();
}
