//! Git graph layout, top to bottom like `git log --graph`: one lane per branch under
//! its name, one commit every other row, ids and tags in a column on the right.

use super::canvas::{Canvas, draw_text};
use super::labels::{WRAP_WIDTH, fit_label};
use super::layout::MAX_CANVAS_CELLS;
use super::parse_git::{CommitKind, GitGraph};
use super::types::Cls;
use super::width::string_width;

fn commit_glyph(kind: CommitKind) -> char {
    match kind {
        CommitKind::Normal => '●',
        CommitKind::Reverse => '✕',
        CommitKind::Highlight => '■',
        CommitKind::Merge => '◉',
        CommitKind::CherryPick => '◎',
    }
}

pub(super) fn layout_git(git: &GitGraph) -> Option<Canvas> {
    let mut lanes: Vec<usize> = (0..git.branches.len()).collect();
    lanes.sort_by(|&a, &b| {
        let key = |i: usize| git.branches[i].order.unwrap_or(i as f64);
        key(a).total_cmp(&key(b)).then(a.cmp(&b))
    });
    let names: Vec<String> = git.branches.iter().map(|b| fit_label(&b.name, WRAP_WIDTH)).collect();
    let mut lane_x = vec![0; git.branches.len()];
    let mut x = 0;
    for &b in &lanes {
        lane_x[b] = x;
        x += (string_width(&names[b]) + 2).max(3);
    }
    let label_x = lanes.last().map_or(0, |&b| lane_x[b]) + 3;

    let labels: Vec<(String, String)> = git
        .commits
        .iter()
        .map(|c| {
            let id = c.id.as_deref().map(|id| fit_label(id, WRAP_WIDTH)).unwrap_or_default();
            let tags: Vec<String> = c.tags.iter().map(|t| format!("[{}]", fit_label(t, WRAP_WIDTH))).collect();
            (id, tags.join(" "))
        })
        .collect();
    let row = |i: usize| 1 + 2 * i;
    let canvas_h = row(git.commits.len() - 1) + 1;
    let canvas_w = labels
        .iter()
        .map(|(id, tags)| label_x + string_width(id) + 1 + string_width(tags))
        .chain(lanes.iter().map(|&b| lane_x[b] + string_width(&names[b])))
        .max()
        .unwrap_or(1);
    if canvas_w * canvas_h > MAX_CANVAS_CELLS {
        return None;
    }

    let mut canvas = Canvas::new(canvas_w, canvas_h);
    for (b, name) in names.iter().enumerate() {
        draw_text(&mut canvas, name, lane_x[b], 0, Cls::EdgeLabel);
    }
    for (i, commit) in git.commits.iter().enumerate() {
        let (x, y) = (lane_x[commit.branch], row(i));
        canvas.set_char(x, y, commit_glyph(commit.kind), Cls::Title);
        let cell = canvas.idx(x, y);
        canvas.occupied[cell] = true;
        let (id, tags) = &labels[i];
        draw_text(&mut canvas, id, label_x, y, Cls::Text);
        let tag_x = if id.is_empty() { label_x } else { label_x + string_width(id) + 1 };
        draw_text(&mut canvas, tags, tag_x, y, Cls::Title);
    }
    for (i, commit) in git.commits.iter().enumerate() {
        let (x, y) = (lane_x[commit.branch], row(i));
        for (n, &p) in commit.parents.iter().enumerate() {
            let (px, py) = (lane_x[git.commits[p].branch], row(p));
            if n == 0 {
                // Fork: leave the parent sideways, then run down the child's lane.
                canvas.seg_h(py, px, x);
                canvas.seg_v(x, py, y);
            } else {
                // Merge: run down the merged lane, then join the child sideways.
                canvas.seg_v(px, py, y);
                canvas.seg_h(y, px, x);
            }
        }
    }
    canvas.finalize_mask();
    if git.bottom_up {
        canvas.flip_vertical();
    }
    Some(canvas)
}
