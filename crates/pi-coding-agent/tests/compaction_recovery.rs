//! Automatic compaction decisions, ported from the `_checkCompaction` cases of upstream
//! `suite/agent-session-compaction.test.ts` and `agent-session-auto-compaction-queue.test.ts`.

mod common;

use common::*;
use pi_ai::providers::faux::FauxProvider;
use pi_ai::{AssistantMessage, Message, Model, StopReason, is_context_overflow, is_recoverable_length};
use pi_coding_agent::compaction::{
    CompactionCheck, CompactionCheckInput, CompactionSettings, check_compaction, exceeds_compaction_threshold,
    find_message_entry_id, omit_recovery_attempt,
};
use pi_coding_agent::session::SessionManager;

const OVERFLOW_FAILED: &str = "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model.";

fn reply(model: &Model, stop: StopReason, error: Option<&str>, total: u64, timestamp: i64) -> AssistantMessage {
    let mut message = AssistantMessage::empty(model);
    message.stop_reason = stop;
    message.error_message = error.map(String::from);
    message.usage = usage(total, 0, 0, 0);
    message.timestamp = timestamp;
    message
}

struct Check<'a> {
    model: Model,
    settings: CompactionSettings,
    entry_id: Option<&'a str>,
    recovery_attempted: bool,
    skip_aborted: bool,
    messages: Vec<Message>,
}

impl Check<'_> {
    fn new(model: Model) -> Self {
        let settings = CompactionSettings::default();
        Check { model, settings, entry_id: None, recovery_attempted: false, skip_aborted: true, messages: Vec::new() }
    }

    fn run(&self, session: &SessionManager, assistant: &AssistantMessage) -> CompactionCheck {
        let input = CompactionCheckInput {
            assistant,
            assistant_entry_id: self.entry_id,
            message_model: Some(&self.model),
            current_model: &self.model,
            settings: &self.settings,
            skip_aborted: self.skip_aborted,
            recovery_attempted: self.recovery_attempted,
            agent_messages: &self.messages,
        };
        check_compaction(session, input)
    }
}

#[test]
fn overflow_errors_recover_once() {
    let session = memory();
    let mut check = Check::new(FauxProvider::model());
    let overflow = reply(&check.model, StopReason::Error, Some("prompt is too long"), 0, pi_ai::now_ms());
    assert_eq!(check.run(&session, &overflow), CompactionCheck::Overflow { will_retry: true });
    check.recovery_attempted = true;
    let exhausted = CompactionCheck::RecoveryExhausted { error_message: OVERFLOW_FAILED.into() };
    assert_eq!(check.run(&session, &overflow), exhausted);

    let other_model = Check::new(FauxProvider::model());
    let input = CompactionCheckInput {
        assistant: &overflow,
        assistant_entry_id: None,
        message_model: None,
        current_model: &other_model.model,
        settings: &other_model.settings,
        skip_aborted: true,
        recovery_attempted: false,
        agent_messages: &[],
    };
    assert_eq!(check_compaction(&session, input), CompactionCheck::Nothing, "another model's overflow is ignored");
}

#[test]
fn successful_overflow_compacts_without_retry() {
    let session = memory();
    let check = Check::new(Model { context_window: 1, max_tokens: 100, ..FauxProvider::model() });
    let answer = reply(&check.model, StopReason::Stop, None, 10, pi_ai::now_ms());
    assert_eq!(check.run(&session, &answer), CompactionCheck::Overflow { will_retry: false });
}

#[test]
fn length_stops_below_the_output_limit_are_recoverable() {
    let session = memory();
    let mut check = Check::new(Model { context_window: 1_000_000, max_tokens: 100, ..FauxProvider::model() });
    let mut truncated = reply(&check.model, StopReason::Length, None, 10, pi_ai::now_ms());
    truncated.usage.output = 50;
    assert!(is_recoverable_length(&truncated, 100) && !is_recoverable_length(&truncated, 50));
    assert!(!is_recoverable_length(&truncated, 0));
    assert_eq!(check.run(&session, &truncated), CompactionCheck::Overflow { will_retry: true });
    check.recovery_attempted = true;
    let exhausted = "Truncated response recovery failed after one compact-and-retry attempt.";
    assert_eq!(check.run(&session, &truncated), CompactionCheck::RecoveryExhausted { error_message: exhausted.into() });
    truncated.usage.output = 100;
    assert_eq!(check.run(&session, &truncated), CompactionCheck::Nothing, "reached the desired limit");
}

#[test]
fn length_stops_filling_the_window_keep_overflow_wording() {
    let session = memory();
    let mut check = Check::new(Model { context_window: 100, max_tokens: 100, ..FauxProvider::model() });
    let full = reply(&check.model, StopReason::Length, None, 100, pi_ai::now_ms());
    assert!(is_context_overflow(&full, Some(100)) && !is_context_overflow(&full, Some(200)));
    assert_eq!(check.run(&session, &full), CompactionCheck::Overflow { will_retry: true });
    check.recovery_attempted = true;
    let exhausted = CompactionCheck::RecoveryExhausted { error_message: OVERFLOW_FAILED.into() };
    assert_eq!(check.run(&session, &full), exhausted);
}

fn compacted_session(model: &Model, before: i64, total: u64) -> (SessionManager, AssistantMessage) {
    let mut session = memory();
    let mut first = user("before compaction");
    if let Message::User(message) = &mut first {
        message.timestamp = before - 1000;
    }
    session.append_message(&first).unwrap();
    let stale = reply(model, StopReason::Stop, None, total, before);
    session.append_message(&Message::Assistant(stale.clone())).unwrap();
    let first_kept = session.entries()[0].id().to_string();
    session.append_compaction("summary", &first_kept, total).unwrap();
    (session, stale)
}

#[test]
fn stale_pre_compaction_usage_is_ignored() {
    let model = FauxProvider::model();
    let (mut session, stale) = compacted_session(&model, pi_ai::now_ms() - 10_000, 610_000);
    session.append_message(&user("after compaction")).unwrap();
    let mut check = Check::new(model);
    check.skip_aborted = false;
    assert_eq!(check.run(&session, &stale), CompactionCheck::Nothing);
}

#[test]
fn error_responses_use_the_last_successful_usage() {
    let session = memory();
    let mut check = Check::new(FauxProvider::model());
    let now = pi_ai::now_ms();
    let success = reply(&check.model, StopReason::Stop, None, 190_000, now);
    let failure = reply(&check.model, StopReason::Error, Some("529 overloaded"), 0, now + 1000);
    check.messages =
        vec![user("hello"), Message::Assistant(success), user("retry"), Message::Assistant(failure.clone())];
    assert_eq!(check.run(&session, &failure), CompactionCheck::Threshold);
    check.messages = vec![user("hello"), Message::Assistant(failure.clone())];
    assert_eq!(check.run(&session, &failure), CompactionCheck::Nothing, "no prior usage");

    let model = FauxProvider::model();
    let (session, kept) = compacted_session(&model, now - 10_000, 190_000);
    let failure = reply(&model, StopReason::Error, Some("529 overloaded"), 0, now);
    check.messages =
        vec![user("kept user"), Message::Assistant(kept), user("new prompt"), Message::Assistant(failure.clone())];
    assert_eq!(check.run(&session, &failure), CompactionCheck::Nothing, "only kept pre-compaction usage");
}

#[test]
fn threshold_respects_settings() {
    let session = memory();
    let mut check = Check::new(Model { context_window: 200_000, ..FauxProvider::model() });
    check.settings.reserve_tokens = 1000;
    let small = reply(&check.model, StopReason::Stop, None, 1_000, pi_ai::now_ms());
    assert_eq!(check.run(&session, &small), CompactionCheck::Nothing);
    let large = reply(&check.model, StopReason::Stop, None, 199_500, pi_ai::now_ms());
    assert_eq!(check.run(&session, &large), CompactionCheck::Threshold);
    check.settings.enabled = false;
    assert_eq!(check.run(&session, &large), CompactionCheck::Nothing);
    check.settings.enabled = true;
    let aborted = reply(&check.model, StopReason::Aborted, None, 199_500, pi_ai::now_ms());
    assert_eq!(check.run(&session, &aborted), CompactionCheck::Nothing);
    check.skip_aborted = false;
    assert_eq!(check.run(&session, &aborted), CompactionCheck::Threshold);
}

#[test]
fn omitted_recovery_responses_leave_the_projection() {
    let model = Model { context_window: 1_000_000, max_tokens: 100, ..FauxProvider::model() };
    let mut session = memory();
    session.append_message(&user("question")).unwrap();
    let mut truncated = reply(&model, StopReason::Length, None, 10, pi_ai::now_ms());
    truncated.usage.output = 50;
    let message = Message::Assistant(truncated.clone());
    session.append_message(&message).unwrap();
    let entry_id = find_message_entry_id(&session, &message).unwrap();
    assert_eq!(session.leaf_id(), Some(entry_id.as_str()));
    let mut check = Check::new(model);
    check.entry_id = Some(&entry_id);
    assert_eq!(check.run(&session, &truncated), CompactionCheck::Overflow { will_retry: true });

    let edits = omit_recovery_attempt(&mut session, &[&entry_id]).unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(roles(&session.build_session_context().messages), ["user"]);
    assert_eq!(check.run(&session, &truncated), CompactionCheck::Nothing, "the omitted response is not projected");
    assert!(omit_recovery_attempt(&mut session, &["missing"]).is_err());
}

#[test]
fn between_turn_threshold_uses_the_projected_branch() {
    let mut session = memory();
    session.append_message(&user(&"x".repeat(4_000))).unwrap();
    let small = Model { context_window: 1_500, ..FauxProvider::model() };
    let settings = CompactionSettings { reserve_tokens: 600, ..CompactionSettings::default() };
    assert!(exceeds_compaction_threshold(&session, &small, &settings));
    let large = Model { context_window: 10_000, ..small.clone() };
    assert!(!exceeds_compaction_threshold(&session, &large, &settings));
    let unknown = Model { context_window: 0, ..small };
    assert!(!exceeds_compaction_threshold(&session, &unknown, &settings));
}
