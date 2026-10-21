//! `pi sessions list`, `rm`, and `prune` through the binary, on session files written into a
//! temporary agent directory.

mod cli_support;
mod session_fixtures;

use serde_json::{Value, json};
use session_fixtures::*;

fn ids(list: &Value) -> Vec<&str> {
    list.as_array().unwrap().iter().map(|session| session["id"].as_str().unwrap()).collect()
}

#[test]
fn lists_this_project_or_every_project_newest_first() {
    let root = Root::new();
    let (work, other) = (root.work(), root.other());
    let mut alpha = Spec::new("aaaa1111-0000-7000-8000-000000000001", &work, now() - 2 * DAY);
    alpha.name = Some("Alpha work");
    alpha.turns = vec![turn("anthropic", "claude-test", 100, 10, 0.5), turn("openai", "gpt-test", 50, 5, 0.25)];
    root.write(&alpha);
    let mut beta = Spec::new("bbbb2222-0000-7000-8000-000000000002", &work, now() - DAY);
    beta.prompt = "Fix the\nparser   bug";
    let beta_path = root.write(&beta);
    root.write(&Spec::new("cccc3333-0000-7000-8000-000000000003", &other, now() - 3 * HOUR));

    let text = stdout(&ok(root.pi(&["sessions"])));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines[0].starts_with("ID") && lines[0].contains("NAME / FIRST PROMPT") && !lines[0].contains("PROJECT"));
    assert!(lines[1].starts_with("bbbb2222  ") && lines[1].ends_with("Fix the parser bug"), "{text}");
    assert!(lines[2].starts_with("aaaa1111  ") && lines[2].contains("$0.75") && lines[2].ends_with("Alpha work"));
    assert_eq!(stdout(&ok(root.pi(&["sessions", "list"]))), text);

    let all = stdout(&ok(root.pi(&["sessions", "--all"])));
    assert!(all.lines().next().unwrap().contains("PROJECT"), "{all}");
    let order: Vec<&str> = all.lines().skip(1).map(|line| &line[..4]).collect();
    assert_eq!(order, ["cccc", "bbbb", "aaaa"]);

    let list = json(&ok(root.pi(&["sessions", "--json"])));
    assert_eq!(ids(&list), [beta.id, alpha.id]);
    let first = &list[1];
    let keys: Vec<&str> = first.as_object().unwrap().keys().map(String::as_str).collect();
    let expected =
        ["id", "shortId", "path", "cwd", "name", "firstMessage", "created", "modified", "messageCount", "cost"];
    assert_eq!(keys, expected);
    assert_eq!(first["shortId"], json!("aaaa1111"));
    assert_eq!((first["name"].clone(), first["firstMessage"].clone()), (json!("Alpha work"), json!("hello")));
    assert_eq!((first["messageCount"].clone(), first["cwd"].clone()), (json!(4), json!(work.display().to_string())));
    assert!((first["cost"].as_f64().unwrap() - 0.75).abs() < 1e-9);
    assert!(first["modified"].as_str().unwrap().ends_with('Z'));
    assert_eq!(list[0]["name"], Value::Null);
    assert_eq!(list[0]["path"], json!(beta_path.display().to_string()));

    let limited = json(&ok(root.pi(&["sessions", "list", "--json", "--limit", "1"])));
    assert_eq!(ids(&limited), [beta.id]);
    let limited_text = stdout(&ok(root.pi(&["sessions", "--limit=1"])));
    assert!(limited_text.contains("(1 of 2 sessions; --limit)"), "{limited_text}");
    let everything = json(&ok(root.pi(&["sessions", "--all", "--json"])));
    assert_eq!(ids(&everything).len(), 3);
    assert_eq!(everything[0]["id"], json!("cccc3333-0000-7000-8000-000000000003"));

    let bad = root.pi(&["sessions", "--limit", "many"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(stderr(&bad).contains("--limit needs a number"));
    let unknown = root.pi(&["sessions", "--older-than", "3d"]);
    assert!(stderr(&unknown).contains("Unknown option --older-than"), "{}", stderr(&unknown));
}

#[test]
fn honors_session_dir_and_reports_an_empty_project() {
    let root = Root::new();
    let flat = root.dir.path().join("flat");
    Spec::new("dddd4444-0000-7000-8000-000000000004", &root.work(), now() - HOUR).write_to(&flat);
    Spec::new("eeee5555-0000-7000-8000-000000000005", &root.other(), now() - HOUR).write_to(&flat);
    assert_eq!(stdout(&ok(root.pi(&["sessions"]))), "No sessions found for this project.\n");
    let flat_arg = flat.display().to_string();
    let local = json(&ok(root.pi(&["sessions", "--session-dir", &flat_arg, "--json"])));
    assert_eq!(ids(&local), ["dddd4444-0000-7000-8000-000000000004"]);
    let all = json(&ok(root.pi(&["sessions", "--all", "--json", "--session-dir", &flat_arg])));
    assert_eq!(ids(&all).len(), 2);
    assert_eq!(json(&ok(root.pi(&["sessions", "--json"]))), json!([]));
    std::fs::write(root.agent().join("settings.json"), json!({"sessionDir": flat_arg}).to_string()).unwrap();
    let from_settings = json(&ok(root.pi(&["sessions", "--json"])));
    assert_eq!(ids(&from_settings), ["dddd4444-0000-7000-8000-000000000004"]);
}

#[test]
fn rm_deletes_by_prefix_or_path_and_refuses_ambiguous_prefixes() {
    let root = Root::new();
    let work = root.work();
    let first = root.write(&Spec::new("abcd1111-0000-7000-8000-000000000001", &work, now() - DAY));
    let second = root.write(&Spec::new("abcd2222-0000-7000-8000-000000000002", &work, now() - 2 * DAY));
    let third = root.write(&Spec::new("ef012345-0000-7000-8000-000000000003", &work, now() - 3 * DAY));
    let elsewhere = root.write(&Spec::new("9999aaaa-0000-7000-8000-000000000004", &root.other(), now() - DAY));

    let ambiguous = root.pi(&["sessions", "rm", "abcd", "--yes"]);
    assert_eq!(ambiguous.status.code(), Some(1));
    let error = stderr(&ambiguous);
    assert!(error.contains("'abcd' matches 2 sessions; use a longer prefix"), "{error}");
    assert!(error.contains("  abcd1111  ") && error.contains("  abcd2222  ") && error.contains("Nothing was deleted"));
    assert!(first.exists() && second.exists());

    let refused = root.pi(&["sessions", "rm", "abcd1"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(stderr(&refused).contains("Refusing to delete 1 session without confirmation"), "{}", stderr(&refused));
    assert!(first.exists());

    let deleted = stdout(&ok(root.pi(&["sessions", "rm", "abcd1", "-y"])));
    assert!(deleted.contains("Deleted abcd1111  ") && deleted.contains("Deleted 1 session, freed "), "{deleted}");
    assert!(!first.exists() && second.exists());

    let by_path = third.display().to_string();
    let output = stdout(&ok(root.pi(&["sessions", "rm", &by_path, "abcd2222-0000-7000-8000-000000000002", "--yes"])));
    assert!(output.contains("Deleted 2 sessions"), "{output}");
    assert!(!second.exists() && !third.exists());

    let other_project = stdout(&ok(root.pi(&["sessions", "rm", "9999", "--yes"])));
    assert!(other_project.contains("Deleted 9999aaaa"), "{other_project}");
    assert!(!elsewhere.exists());

    let missing = root.pi(&["sessions", "rm", "nope", "--yes"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(stderr(&missing).contains("No session found matching 'nope'"));
    let not_session = root.dir.path().join("notes.jsonl");
    std::fs::write(&not_session, "{\"type\":\"note\"}\n").unwrap();
    let refused_file = root.pi(&["sessions", "rm", &not_session.display().to_string(), "--yes"]);
    assert!(stderr(&refused_file).contains("Not a session file"), "{}", stderr(&refused_file));
    assert!(not_session.exists());
    let empty = root.pi(&["sessions", "rm"]);
    assert!(empty.status.code() == Some(1) && stderr(&empty).contains("No session given"));
}

#[test]
fn prune_lists_with_dry_run_then_deletes_old_sessions() {
    let root = Root::new();
    let (work, other) = (root.work(), root.other());
    let old = root.write(&Spec::new("0001aaaa-0000-7000-8000-000000000001", &work, now() - 40 * DAY));
    let older = root.write(&Spec::new("0002bbbb-0000-7000-8000-000000000002", &work, now() - 35 * DAY));
    let fresh = root.write(&Spec::new("0003cccc-0000-7000-8000-000000000003", &work, now() - DAY));
    let elsewhere = root.write(&Spec::new("0004dddd-0000-7000-8000-000000000004", &other, now() - 50 * DAY));
    let bytes = std::fs::metadata(&old).unwrap().len() + std::fs::metadata(&older).unwrap().len();
    let size = pi_coding_agent::tools::truncate::format_size(bytes as usize);

    let dry = stdout(&ok(root.pi(&["sessions", "prune", "--older-than", "30d", "--dry-run"])));
    assert!(dry.contains("  0001aaaa  ") && dry.contains("  0002bbbb  ") && !dry.contains("0003cccc"), "{dry}");
    assert!(dry.contains(&format!("Would delete 2 sessions in this project, freeing {size}.")), "{dry}");
    assert!(old.exists() && older.exists());
    let half_day = stdout(&ok(root.pi(&["sessions", "prune", "--older-than", "12h", "--dry-run"])));
    assert!(half_day.contains("Would delete 3 sessions"), "{half_day}");
    let none = stdout(&ok(root.pi(&["sessions", "prune", "--older-than", "1w8w", "--dry-run"])));
    assert_eq!(none, "No sessions in this project are older than that.\n");

    for (args, message) in [
        (vec!["--older-than", "30x"], "Invalid duration \"30x\""),
        (vec!["--older-than", "d"], "Invalid duration \"d\""),
        (vec!["--older-than", "30"], "Invalid duration \"30\""),
        (vec![], "prune needs --older-than"),
        (vec!["--older-than", "30d"], "Refusing to delete 2 sessions without confirmation"),
    ] {
        let output = root.pi(&[&["sessions", "prune"], args.as_slice()].concat());
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(stderr(&output).contains(message), "{args:?}: {}", stderr(&output));
    }
    assert!(old.exists() && older.exists());

    let pruned = stdout(&ok(root.pi(&["sessions", "prune", "--older-than", "30d", "--yes"])));
    assert!(pruned.contains(&format!("Deleted 2 sessions, freed {size}.")), "{pruned}");
    assert!(!old.exists() && !older.exists() && fresh.exists() && elsewhere.exists());
    let everywhere = stdout(&ok(root.pi(&["sessions", "prune", "--older-than", "30d", "--all", "--yes"])));
    assert!(everywhere.contains("Deleted 0004dddd") && everywhere.contains("Deleted 1 session"), "{everywhere}");
    assert!(!elsewhere.exists() && fresh.exists());
}

#[test]
fn sessions_help_and_unknown_commands() {
    let root = Root::new();
    let help = stdout(&ok(root.pi(&["sessions", "--help"])));
    assert!(help.contains("sessions prune --older-than <duration>") && help.contains("--dry-run"), "{help}");
    assert_eq!(stdout(&ok(root.pi(&["sessions", "rm", "--help"]))), help);
    let unknown = root.pi(&["sessions", "frobnicate"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(stderr(&unknown).contains("Unknown command frobnicate"));
    let top = stdout(&ok(root.pi(&["--help"])));
    assert!(top.contains(" sessions [list|rm|prune]  "), "{top}");
}
