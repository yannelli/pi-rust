//! `pi stats` through the binary, on session files written into a temporary agent directory.

mod cli_support;
mod session_fixtures;

use serde_json::{Value, json};
use session_fixtures::*;

fn cost(value: &Value) -> f64 {
    value["cost"].as_f64().unwrap()
}

fn assert_cost(value: &Value, expected: f64) {
    assert!((cost(value) - expected).abs() < 1e-9, "expected cost {expected}: {value:#}");
}

fn names(rows: &Value) -> Vec<&str> {
    rows.as_array().unwrap().iter().map(|row| row["name"].as_str().unwrap()).collect()
}

/// Three sessions here (2, 10, and 20 days old) and one in another project.
fn fixture() -> Root {
    let root = Root::new();
    let (work, other) = (root.work(), root.other());
    let mut recent = Spec::new("11110000-0000-7000-8000-000000000001", &work, noon_days_ago(2));
    let cached = Turn { cache_read: 1000, cache_write: 50, ..turn("anthropic", "claude-x", 100, 10, 0.10) };
    recent.turns = vec![cached, turn("openai", "gpt-y", 200, 20, 0.20)];
    root.write(&recent);
    let mut middle = Spec::new("22220000-0000-7000-8000-000000000002", &work, noon_days_ago(10));
    middle.turns = vec![turn("anthropic", "claude-x", 300, 30, 0.30)];
    root.write(&middle);
    let mut old = Spec::new("33330000-0000-7000-8000-000000000003", &work, noon_days_ago(20));
    old.turns = vec![turn("openai", "gpt-y", 400, 40, 0.40)];
    root.write(&old);
    let mut elsewhere = Spec::new("44440000-0000-7000-8000-000000000004", &other, noon_days_ago(1));
    elsewhere.turns = vec![turn("anthropic", "claude-x", 1000, 100, 1.0)];
    root.write(&elsewhere);
    root
}

#[test]
fn totals_by_model_provider_and_day_as_json() {
    let root = fixture();
    let report = json(&ok(root.pi(&["stats", "--json"])));
    assert_eq!(report["scope"], json!("project"));
    assert_eq!(report["since"], Value::Null);
    assert_eq!((report["sessions"].clone(), report["prompts"].clone()), (json!(3), json!(4)));
    assert_eq!(report["assistantTurns"], json!(4));
    let tokens = json!({"input": 1000, "output": 100, "cacheRead": 1000, "cacheWrite": 50, "total": 2150});
    assert_eq!(report["tokens"], tokens);
    assert_cost(&report, 1.0);

    assert_eq!(names(&report["byModel"]), ["openai/gpt-y", "anthropic/claude-x"]);
    let gpt = &report["byModel"][0];
    assert_eq!((gpt["sessions"].clone(), gpt["assistantTurns"].clone()), (json!(2), json!(2)));
    assert_eq!(gpt["tokens"], json!({"input": 600, "output": 60, "cacheRead": 0, "cacheWrite": 0, "total": 660}));
    assert_cost(gpt, 0.6);
    let claude = &report["byModel"][1];
    assert_eq!(claude["tokens"]["cacheRead"], json!(1000));
    assert_cost(claude, 0.4);
    assert_eq!(names(&report["byProvider"]), ["openai", "anthropic"]);
    assert_cost(&report["byProvider"][1], 0.4);

    let days = report["byDay"].as_array().unwrap();
    let dates: Vec<&str> = days.iter().map(|day| day["date"].as_str().unwrap()).collect();
    assert_eq!(dates, [utc_date(noon_days_ago(10)), utc_date(noon_days_ago(2))], "20 days ago is not in the table");
    assert_eq!((days[1]["sessions"].clone(), days[1]["prompts"].clone()), (json!(1), json!(2)));
    assert_eq!((days[1]["assistantTurns"].clone(), days[1]["tokens"]["total"].clone()), (json!(2), json!(1380)));
    assert_cost(&days[0], 0.3);
    assert_cost(&days[1], 0.3);
}

#[test]
fn since_and_all_change_what_is_counted() {
    let root = fixture();
    let recent = json(&ok(root.pi(&["stats", "--since", "5d", "--json"])));
    assert_eq!((recent["sessions"].clone(), recent["prompts"].clone()), (json!(1), json!(2)));
    assert_eq!(recent["assistantTurns"], json!(2));
    assert_cost(&recent, 0.3);
    assert_eq!(names(&recent["byModel"]), ["openai/gpt-y", "anthropic/claude-x"]);
    assert_eq!(recent["byDay"].as_array().unwrap().len(), 1);
    let since = recent["since"].as_str().unwrap();
    assert!(since.ends_with('Z') && since < utc_date(now()).as_str(), "{since}");
    let all_time = json(&ok(root.pi(&["stats", "--since", "52w", "--json"])));
    assert_eq!(all_time["sessions"], json!(3));

    let everywhere = json(&ok(root.pi(&["stats", "--all", "--json"])));
    assert_eq!((everywhere["scope"].clone(), everywhere["sessions"].clone()), (json!("all"), json!(4)));
    assert_cost(&everywhere, 2.0);
    assert_eq!(names(&everywhere["byModel"]), ["anthropic/claude-x", "openai/gpt-y"]);
    assert_eq!(everywhere["byModel"][0]["sessions"], json!(3));
    let recent_everywhere = json(&ok(root.pi(&["stats", "--all", "--since", "36h", "--json"])));
    assert_eq!(recent_everywhere["sessions"], json!(1));
    assert_cost(&recent_everywhere, 1.0);

    let bad = root.pi(&["stats", "--since", "soon"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(stderr(&bad).contains("Invalid duration \"soon\""), "{}", stderr(&bad));
}

#[test]
fn prints_a_compact_text_report() {
    let root = fixture();
    let text = stdout(&ok(root.pi(&["stats"])));
    assert!(text.starts_with("Project "), "{text}");
    for expected in [
        "Sessions         3\n",
        "Prompts          4\n",
        "Assistant turns  4\n",
        "Tokens           1.0k in, 100 out, 1.0k cache read, 50 cache write\n",
        "Cost             $1.00\n",
        "\nBy model\n  MODEL ",
        "\nBy provider\n  PROVIDER ",
        "\nLast 14 days\n  DATE ",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in\n{text}");
    }
    let gpt = text.lines().find(|line| line.starts_with("  openai/gpt-y ")).unwrap();
    assert!(gpt.ends_with("  $0.60"), "{gpt}");
    let day = text.lines().find(|line| line.starts_with(&format!("  {}", utc_date(noon_days_ago(2))))).unwrap();
    assert!(day.ends_with("1.4k  $0.30"), "{day}");

    let empty = Root::new();
    let nothing = stdout(&ok(empty.pi(&["stats", "--since", "1d"])));
    assert!(nothing.contains("Sessions         0\n") && !nothing.contains("By model"), "{nothing}");
    let help = stdout(&ok(empty.pi(&["stats", "--help"])));
    assert!(help.contains("stats [--all] [--since <duration>] [--json]"), "{help}");
}
