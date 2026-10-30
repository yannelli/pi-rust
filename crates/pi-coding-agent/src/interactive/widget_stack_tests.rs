use pi_tui::{strip_terminal_sequences, visible_width};

use super::{WidgetsView, budget, fit};

fn widget(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| line.to_string()).collect()
}

fn plain(fitted: &[Vec<String>]) -> Vec<Vec<String>> {
    fitted.iter().map(|lines| lines.iter().map(|line| strip_terminal_sequences(line)).collect()).collect()
}

#[test]
fn the_budget_is_a_third_of_the_rows_with_a_floor() {
    assert_eq!(budget(0), None);
    assert_eq!(budget(30), Some(10));
    assert_eq!(budget(6), Some(3));
}

#[test]
fn widgets_within_the_budget_show_in_full() {
    let widgets = vec![widget(&["a", "b"]), widget(&["c"])];
    let (fitted, collapsed) = fit(widgets.clone(), WidgetsView::Auto, Some(3), 40, "alt+w");
    assert_eq!((fitted, collapsed), (widgets, false));
}

#[test]
fn widgets_over_the_budget_show_their_first_line_and_a_count() {
    let widgets = vec![widget(&["", " Todo: 3 open   ", "x", "y"]), widget(&["one line"]), widget(&["bg", "t1"])];
    let (fitted, collapsed) = fit(widgets, WidgetsView::Auto, Some(4), 40, "alt+w");
    assert!(collapsed);
    assert_eq!(
        plain(&fitted),
        [vec![" Todo: 3 open +3 lines · alt+w".to_string()], vec!["one line".into()], vec!["bg +1 line".into()]]
    );
}

#[test]
fn summaries_fit_the_width() {
    let widgets = vec![widget(&["a long first line that does not fit", "more"])];
    let (fitted, _) = fit(widgets, WidgetsView::Collapsed, None, 20, "");
    assert!(visible_width(&fitted[0][0]) <= 20, "{:?}", fitted[0][0]);
    assert!(strip_terminal_sequences(&fitted[0][0]).ends_with("+1 line"), "{:?}", fitted[0][0]);
}

#[test]
fn widgets_past_the_budget_give_way_to_one_line() {
    let widgets = vec![widget(&["a", "1"]), Vec::new(), widget(&["b", "2"]), widget(&["c", "3"]), widget(&["d", "4"])];
    let (fitted, _) = fit(widgets, WidgetsView::Auto, Some(3), 40, "");
    assert_eq!(
        plain(&fitted),
        [
            vec!["a +1 line".to_string()],
            vec![],
            vec!["b +1 line".into()],
            vec![" +2 more widgets · /widgets".into()],
            vec![]
        ]
    );
}

#[test]
fn the_view_overrides_the_budget() {
    let widgets = vec![widget(&["a", "b", "c", "d"])];
    let (expanded, collapsed) = fit(widgets.clone(), WidgetsView::Expanded, Some(3), 40, "");
    assert_eq!((expanded, collapsed), (widgets.clone(), false));
    let (fitted, collapsed) = fit(widgets, WidgetsView::Collapsed, None, 40, "");
    assert_eq!((plain(&fitted), collapsed), (vec![vec!["a +3 lines".to_string()]], true));
}
