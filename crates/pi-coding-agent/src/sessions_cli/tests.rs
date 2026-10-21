//! Unit tests for `pi sessions`: durations, flags, short ids, and the terminal confirmation.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::list::short_id_len;
use super::*;
use crate::session::{format_iso, session_dir};

const HOUR: i64 = 3_600_000;

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

#[test]
fn parses_durations() {
    assert_eq!(parse_duration("30d"), Ok(30 * 24 * HOUR));
    assert_eq!(parse_duration("12h"), Ok(12 * HOUR));
    assert_eq!(parse_duration("90m"), Ok(90 * 60_000));
    assert_eq!(parse_duration("2w"), Ok(14 * 24 * HOUR));
    assert_eq!(parse_duration("1d12h"), Ok(36 * HOUR));
    assert_eq!(parse_duration(" 45s "), Ok(45_000));
    assert_eq!(parse_duration("0d"), Ok(0));
    for bad in ["", "30", "d", "3x", "1.5d", "-1d", "12h30", "99999999999999999999d"] {
        let error = parse_duration(bad).unwrap_err();
        assert!(error.starts_with(&format!("Invalid duration \"{bad}\"")), "{bad}: {error}");
    }
}

#[test]
fn parses_only_the_flags_a_command_takes() {
    let flags = parse_flags(&strings(&["a", "--all", "-y", "--limit=3", "--", "--b"]), RM_FLAGS);
    assert!(flags.is_err(), "rm takes no --limit");
    let flags = parse_flags(&strings(&["a", "--all", "-y", "--session-dir", "d", "--", "--b"]), RM_FLAGS).unwrap();
    let expected = Flags {
        all: true,
        yes: true,
        session_dir: Some("d".into()),
        positional: strings(&["a", "--b"]),
        ..Flags::default()
    };
    assert_eq!(flags, expected);
    let flags = parse_flags(&strings(&["--older-than", "2d", "--dry-run"]), PRUNE_FLAGS).unwrap();
    assert_eq!((flags.older_than, flags.dry_run), (Some(48 * HOUR), true));
    let flags = parse_flags(&strings(&["--limit=1", "--json", "-h"]), LIST_FLAGS).unwrap();
    assert_eq!((flags.limit, flags.json, flags.help), (Some(1), true, true));
    assert_eq!(parse_flags(&strings(&["--json"]), RM_FLAGS), Err("Unknown option --json".into()));
    assert_eq!(parse_flags(&strings(&["--limit"]), LIST_FLAGS), Err("--limit needs a value".into()));
    assert_eq!(parse_flags(&strings(&["--all=yes"]), LIST_FLAGS), Err("--all takes no value".into()));
}

#[test]
fn short_ids_grow_until_they_tell_sessions_apart() {
    assert_eq!(short_id_len(["0199a1b2-c3d4", "0299a1b2-c3d4"]), 8);
    assert_eq!(short_id_len(["0199a1b2-c3d4-1", "0199a1b2-c3d5-2", "x"]), 13);
    assert_eq!(short_id_len(Vec::<&str>::new()), 8);
}

/// A session file in the project folder of `cwd`: header, then `lines` as entries.
fn write_session(agent: &Path, cwd: &Path, id: &str, at: i64, lines: &[Value]) -> PathBuf {
    let dir = session_dir(agent, cwd);
    std::fs::create_dir_all(&dir).unwrap();
    let header = json!({"type": "session", "version": 3, "id": id, "timestamp": format_iso(at),
        "cwd": cwd.display().to_string()});
    let mut text = format!("{header}\n");
    for (index, line) in lines.iter().enumerate() {
        let mut line = line.clone();
        line["id"] = json!(format!("e{index}"));
        line.as_object_mut().unwrap().entry("timestamp").or_insert(json!(format_iso(at)));
        text.push_str(&format!("{line}\n"));
    }
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, text).unwrap();
    path
}

fn usage(input: u64, cost: f64) -> Value {
    json!({"input": input, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": input + 1,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": cost}})
}

fn user(text: &str, at: i64) -> Value {
    json!({"type": "message", "message": {"role": "user", "content": text, "timestamp": at}})
}

fn assistant(provider: &str, model: &str, input: u64, cost: f64, at: i64) -> Value {
    json!({"type": "message", "message": {"role": "assistant", "content": [], "api": "x", "provider": provider,
        "model": model, "usage": usage(input, cost), "stopReason": "stop", "timestamp": at}})
}

#[test]
fn rm_and_prune_ask_on_a_terminal() {
    let root = tempfile::tempdir().unwrap();
    let (agent, cwd) = (root.path().join("agent"), root.path().join("work"));
    let at = pi_ai::now_ms() - 48 * HOUR;
    let lines = [user("hello", at), assistant("p", "m", 10, 0.5, at + 1)];
    let path = write_session(&agent, &cwd, "abcd1234-0000", at, &lines);

    let mut questions = Vec::new();
    let mut decline = |question: &str| {
        questions.push(question.to_string());
        false
    };
    let declined = run_sessions(&strings(&["rm", "abcd"]), &cwd, &agent, Some(&mut decline));
    assert_eq!((declined.code, declined.stdout.as_str()), (0, "Aborted.\n"));
    assert!(path.exists());
    assert!(questions[0].starts_with("  abcd1234  ") && questions[0].contains("  hello  ("), "{questions:?}");
    assert!(questions[0].ends_with("\nDelete 1 session? [y/N] "), "{questions:?}");

    let mut accept = |_: &str| true;
    let pruned = run_sessions(&strings(&["prune", "--older-than", "1d"]), &cwd, &agent, Some(&mut accept));
    assert_eq!(pruned.code, 0, "{pruned:?}");
    assert!(pruned.stdout.contains("Deleted 1 session, freed "), "{pruned:?}");
    assert!(!path.exists());
}
