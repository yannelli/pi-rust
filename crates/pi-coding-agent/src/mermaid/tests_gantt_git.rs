//! Gantt charts and git graphs, which this port draws and upstream does not.

use super::dates::{civil_from_days, days_from_civil, format_date, parse_date, parse_duration};
use super::*;

fn plain(src: &str) -> Vec<String> {
    let art = render(src).expect("diagram renders");
    assert!(art.warnings.is_empty(), "{:?}", art.warnings);
    art.plain
}

const LAUNCH_PLAN: &str = "gantt
    title Launch plan
    dateFormat YYYY-MM-DD
    section Build
    API           :a1, 2025-01-06, 10d
    Frontend      :a2, after a1, 12d
    section Ship
    QA            :crit, after a2, 5d
    Release       :milestone, after a2, 0d
";

#[test]
fn gantt_draws_sections_bars_and_milestones() {
    assert_eq!(diagram_kind(LAUNCH_PLAN), Some(DiagramKind::Gantt));
    assert_eq!(
        plain(LAUNCH_PLAN),
        [
            "Launch plan",
            "",
            "            01-06  01-13  01-20  01-27",
            "            ┬──────┬──────┬──────┬──────",
            "Build",
            "  API       ██████████",
            "  Frontend            ████████████",
            "Ship",
            "  QA                              ▓▓▓▓▓",
            "  Release                         ◆",
        ]
    );
}

/// Column of the first bar glyph in a task row and the bar's length.
fn bar(row: &str) -> (usize, usize) {
    let is_bar = |c: char| matches!(c, '█' | '▓' | '▒' | '░');
    let start = row.chars().position(is_bar).expect("row has a bar");
    (start, row.chars().skip(start).take_while(|&c| is_bar(c)).count())
}

#[test]
fn gantt_resolves_forward_references_and_until() {
    // A 4-day span gets 12 columns per day.
    let src = "gantt\n dateFormat YYYY-MM-DD\n B :b, after a, 2d\n A :a, 2025-03-01, 2d\n C :c, 2025-03-01, until b";
    let rows = plain(src);
    assert_eq!(rows[0], "   03-01       03-02       03-03       03-04       03-05");
    assert_eq!([bar(&rows[2]), bar(&rows[3]), bar(&rows[4])], [(27, 24), (3, 24), (3, 24)]);
}

#[test]
fn gantt_without_dates_counts_days() {
    let rows = plain("gantt\n First : done, 3d\n Second : active, 2d");
    assert_eq!(rows[0], "        0d       1d       2d       3d       4d       5d");
    assert!(rows[2].starts_with("First   ░"));
    assert!(rows[3].starts_with("Second  ") && rows[3].contains('▒'));
    assert_eq!([bar(&rows[2]), bar(&rows[3])], [(8, 27), (35, 18)]);
}

#[test]
fn gantt_excludes_weekends() {
    // Friday plus the next two working days ends on Wednesday: five calendar days.
    let src = "gantt\n dateFormat YYYY-MM-DD\n excludes weekends\n T : 2025-01-03, 3d\n U : 1d";
    let rows = plain(src);
    assert_eq!([bar(&rows[2]), bar(&rows[3])], [(3, 40), (43, 8)]);
}

#[test]
fn gantt_rejects_bad_tasks() {
    assert_eq!(render("gantt\n A : after missing, 1d"), None);
    assert_eq!(render("gantt\n A :a, after b, 1d\n B :b, after a, 1d"), None);
    assert_eq!(render("gantt\n A : 2025-13-01, 1d"), None);
    assert_eq!(render("gantt\n title Only a title"), None);
    // A half-typed last task is dropped while the rest still draws.
    let art = render("gantt\n A : 1d\n B : cr").expect("salvaged");
    assert_eq!(art.warnings, ["dropped, unreadable final line: \"B : cr\""]);
}

const GIT: &str = r#"gitGraph
    commit
    branch feature
    checkout feature
    commit
    commit
    checkout main
    merge feature
    commit tag: "v1.0"
"#;

#[test]
fn git_graph_draws_lanes_forks_and_merges() {
    assert_eq!(diagram_kind(GIT), Some(DiagramKind::GitGraph));
    assert_eq!(diagram_kind("gitGraph TB:"), Some(DiagramKind::GitGraph));
    assert_eq!(
        plain(GIT),
        [
            "main  feature",
            "●─────┐",
            "│     │",
            "│     ●",
            "│     │",
            "│     ●",
            "│     │",
            "◉─────┘",
            "│",
            "●        [v1.0]",
        ]
    );
}

#[test]
fn git_graph_orders_lanes_and_draws_bottom_up() {
    let src = r#"gitGraph BT:
        commit id: "init"
        branch b order: 2
        branch a order: 1
        commit type: HIGHLIGHT
        checkout main
        merge a
        checkout b
        merge main
        cherry-pick id: "init""#;
    assert_eq!(
        plain(src),
        [
            "         ◎  cherry-pick: init",
            "         │",
            "┌────────◉",
            "│        │",
            "◉─────┐  │",
            "│     │  │",
            "│     ■  │",
            "│     │  │",
            "●─────┴──┘  init",
            "main  a  b",
        ]
    );
}

#[test]
fn git_graph_rejects_impossible_histories() {
    for body in [
        "checkout nope",
        "branch main",
        "merge main",
        "commit id: \"x\"\n commit id: \"x\"",
        "cherry-pick id: \"x\"",
        "commit type: BOLD",
    ] {
        let src = format!("gitGraph\n commit id: \"x\"\n {body}\n commit");
        assert_eq!(render(&src), None, "{body}");
    }
    assert_eq!(render("gitGraph"), None);
    let art = render("gitGraph\n commit\n checkout nope").expect("salvaged");
    assert_eq!(art.warnings, ["dropped, unreadable final line: \"checkout nope\""]);
}

#[test]
fn dates_round_trip() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    assert_eq!(parse_date("2024-02-30", "YYYY-MM-DD"), None);
    assert_eq!(parse_date("2025-01-06x", "YYYY-MM-DD"), None);
    let t = parse_date("06.01.25 13:30", "DD.MM.YY HH:mm").expect("parses");
    assert_eq!(format_date(t, "%a %e %b %Y %H:%M (%j)"), "Mon  6 Jan 2025 13:30 (006)");
    assert_eq!(parse_date("86400", "X"), Some(1.0));
    assert_eq!(parse_duration("1.5h"), Some(1.5 / 24.0));
    assert_eq!(parse_duration("2w"), Some(14.0));
    assert_eq!(parse_duration("3x"), None);
}
