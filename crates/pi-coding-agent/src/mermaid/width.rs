//! Display width measured per grapheme cluster (upstream `width.ts`). A box is sized
//! for exactly the clusters painted into it; clustering follows UAX #29.

use unicode_segmentation::UnicodeSegmentation;

use super::width_data::WIDTH_RUNS;

const VS16: u32 = 0xfe0f;

fn is_regional_indicator(cp: u32) -> bool {
    (0x1f1e6..=0x1f1ff).contains(&cp)
}

/// Width of one code point; the table covers the whole code point space.
fn code_point_width(cp: u32) -> usize {
    let run = WIDTH_RUNS.partition_point(|&entry| entry >> 2 <= cp);
    match run.checked_sub(1) {
        Some(i) => (WIDTH_RUNS[i] & 3) as usize,
        None => 1,
    }
}

/// Columns occupied by one grapheme cluster: the widest code point wins, and a VS16
/// emoji selector or a regional indicator pair (a flag) forces two columns.
pub(super) fn cluster_width(cluster: &str) -> usize {
    let mut width = 0;
    let mut vs16 = false;
    let mut regional = 0;
    for ch in cluster.chars() {
        let cp = u32::from(ch);
        if cp == VS16 {
            vs16 = true;
        }
        if is_regional_indicator(cp) {
            regional += 1;
        }
        width = width.max(code_point_width(cp));
    }
    if vs16 || regional >= 2 { 2 } else { width }
}

/// Grapheme clusters paired with their display width.
pub(super) fn measured(s: &str) -> impl Iterator<Item = (&str, usize)> {
    s.graphemes(true).map(|g| (g, cluster_width(g)))
}

/// Display columns of a string.
pub(super) fn string_width(s: &str) -> usize {
    s.graphemes(true).map(cluster_width).sum()
}
