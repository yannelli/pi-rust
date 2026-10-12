//! Port of upstream `test/program-status.test.ts` (#10607).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use pi_tui::program_status::{
    BlockedKind, PROGRAM_STATUS_QUERY, ProgramState, ProgramStatus, format_program_status, is_program_status_reply,
};
use regex::Regex;

fn decode_message(sequence: &str) -> Option<String> {
    let pattern = Regex::new(r":msg=([A-Za-z0-9+/=]*)").unwrap();
    let encoded = pattern.captures(sequence)?.get(1)?.as_str().to_owned();
    Some(String::from_utf8(STANDARD.decode(encoded).unwrap()).unwrap())
}

fn status(state: ProgramState) -> ProgramStatus {
    ProgramStatus::new(state)
}

#[test]
fn encodes_state_app_kind_and_a_base64_message() {
    let blocked = ProgramStatus {
        app: Some("pi".to_owned()),
        kind: Some(BlockedKind::Permission),
        message: Some("Allow bash?".to_owned()),
        ..status(ProgramState::Blocked)
    };
    assert_eq!(
        format_program_status(&blocked),
        format!("\x1b]7501;state=blocked:app=pi:kind=permission:msg={}\x1b\\", STANDARD.encode("Allow bash?"))
    );
    assert_eq!(format_program_status(&status(ProgramState::Clear)), "\x1b]7501;state=clear\x1b\\");
}

#[test]
fn omits_kind_outside_blocked_invalid_app_names_and_empty_messages() {
    let working = ProgramStatus {
        app: Some("my app".to_owned()),
        kind: Some(BlockedKind::Auth),
        message: Some(" \n ".to_owned()),
        ..status(ProgramState::Working)
    };
    assert_eq!(format_program_status(&working), "\x1b]7501;state=working\x1b\\");
    let long_app = ProgramStatus { app: Some("a".repeat(32)), ..status(ProgramState::Idle) };
    assert!(Regex::new(r":app=a{32}\x1b").unwrap().is_match(&format_program_status(&long_app)));
    let too_long_app = ProgramStatus { app: Some("a".repeat(33)), ..status(ProgramState::Idle) };
    assert!(!format_program_status(&too_long_app).contains("app="));
}

#[test]
fn replaces_control_characters_which_make_terminals_discard_the_report() {
    let error =
        ProgramStatus { message: Some("first\nsecond\x1b[31m\u{9b}third\t".to_owned()), ..status(ProgramState::Error) };
    assert_eq!(decode_message(&format_program_status(&error)).as_deref(), Some("first second [31m third"));
}

#[test]
fn cuts_long_messages_at_a_utf8_boundary_within_the_spec_limits() {
    let working =
        ProgramStatus { app: Some("pi".to_owned()), message: Some("é".repeat(2000)), ..status(ProgramState::Working) };
    let sequence = format_program_status(&working);
    let message = decode_message(&sequence).unwrap();
    assert_eq!(message, "é".repeat(1024));
    assert!(message.len() <= 2048);
    assert!(sequence.len() <= 4096);
}

#[test]
fn accepts_the_query_echo_with_either_terminator_and_future_pairs() {
    assert!(is_program_status_reply("\x1b]7501;?\x1b\\"));
    assert!(is_program_status_reply("\x1b]7501;?\x07"));
    assert!(is_program_status_reply("\x1b]7501;?version=2\x1b\\"));
    assert!(!is_program_status_reply("\x1b]7501;state=idle\x1b\\"));
    assert!(!is_program_status_reply("\x1b]11;rgb:0000/0000/0000\x07"));
    assert!(is_program_status_reply(PROGRAM_STATUS_QUERY));
}
