//! Matching, replacement, and diff helpers for the edit tool.

use anyhow::{anyhow, bail};
use unicode_normalization::UnicodeNormalization;

pub use pi_durable::diff::{
    DEFAULT_CONTEXT_LINES, EditDiff, generate_diff_string, generate_edit_diff, generate_unified_patch,
};
pub use pi_durable::tools::edit_diff::spawn_edit_diff;

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

pub fn detect_line_ending(content: &str) -> &'static str {
    match (content.find("\r\n"), content.find('\n')) {
        (Some(crlf), Some(lf)) if crlf < lf => "\r\n",
        _ => "\n",
    }
}

pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" { text.replace('\n', "\r\n") } else { text.to_string() }
}

/// NFKC, strip trailing whitespace per line, and map smart quotes, dashes, and special spaces to ASCII.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let trimmed = nfkc.split('\n').map(str::trim_end).collect::<Vec<_>>().join("\n");
    trimmed
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

#[derive(Clone, Debug)]
struct Replacement {
    edit_index: usize,
    start: usize,
    len: usize,
    new_text: String,
}

/// Index and length of `old` in `content`, falling back to fuzzy-normalized matching.
fn fuzzy_find(content: &str, old: &str) -> Option<(usize, usize, bool)> {
    if let Some(i) = content.find(old) {
        return Some((i, old.len(), false));
    }
    let fuzzy_old = normalize_for_fuzzy_match(old);
    normalize_for_fuzzy_match(content).find(&fuzzy_old).map(|i| (i, fuzzy_old.len(), true))
}

fn count_occurrences(content: &str, old: &str) -> usize {
    let fuzzy_old = normalize_for_fuzzy_match(old);
    if fuzzy_old.is_empty() {
        return 0;
    }
    normalize_for_fuzzy_match(content).matches(&fuzzy_old).count()
}

fn apply_replacements(content: &str, replacements: &[Replacement], offset: usize) -> String {
    let mut result = content.to_string();
    for r in replacements.iter().rev() {
        let start = r.start - offset;
        result.replace_range(start..start + r.len, &r.new_text);
    }
    result
}

fn line_spans(content: &str) -> Vec<(usize, usize)> {
    let mut offset = 0;
    content
        .split_inclusive('\n')
        .map(|line| {
            let span = (offset, offset + line.len());
            offset = span.1;
            span
        })
        .collect()
}

fn replacement_line_range(lines: &[(usize, usize)], r: &Replacement) -> anyhow::Result<(usize, usize)> {
    let end = r.start + r.len;
    let start_line = lines
        .iter()
        .position(|(s, e)| r.start >= *s && r.start < *e)
        .ok_or_else(|| anyhow!("Replacement range is outside the base content."))?;
    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].1 < end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        bail!("Replacement range is outside the base content.");
    }
    Ok((start_line, end_line + 1))
}

/// Apply replacements matched against `base` (a normalized view of `original`), rewriting only
/// the touched lines from `base` and copying every other line from `original`.
fn apply_preserving_unchanged_lines(
    original: &str,
    base: &str,
    replacements: &[Replacement],
) -> anyhow::Result<String> {
    let original_lines: Vec<&str> = original.split_inclusive('\n').collect();
    let base_lines = line_spans(base);
    if original_lines.len() != base_lines.len() {
        bail!("Cannot preserve unchanged lines because the base content has a different line count.");
    }
    let mut sorted = replacements.to_vec();
    sorted.sort_by_key(|r| r.start);
    let mut groups: Vec<(usize, usize, Vec<Replacement>)> = Vec::new();
    for r in sorted {
        let (start, end) = replacement_line_range(&base_lines, &r)?;
        if let Some(current) = groups.last_mut().filter(|g| start < g.1) {
            current.1 = current.1.max(end);
            current.2.push(r);
            continue;
        }
        groups.push((start, end, vec![r]));
    }

    let mut result = String::new();
    let mut original_index = 0;
    for (start, end, group) in &groups {
        result.extend(original_lines[original_index..*start].iter().copied());
        let group_start = base_lines[*start].0;
        let group_end = base_lines[end - 1].1;
        result.push_str(&apply_replacements(&base[group_start..group_end], group, group_start));
        original_index = *end;
    }
    result.extend(original_lines[original_index..].iter().copied());
    Ok(result)
}

fn edit_ref(path: &str, i: usize, total: usize, single: &str, multi: &str) -> String {
    if total == 1 { format!("{single} in {path}") } else { format!("edits[{i}]{multi} in {path}") }
}

/// Apply edits to LF-normalized content. Returns `(base_content, new_content)`.
pub fn apply_edits_to_normalized_content(
    content: &str,
    edits: &[Edit],
    path: &str,
) -> anyhow::Result<(String, String)> {
    let edits: Vec<Edit> = edits
        .iter()
        .map(|e| Edit { old_text: normalize_to_lf(&e.old_text), new_text: normalize_to_lf(&e.new_text) })
        .collect();
    let total = edits.len();
    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            if total == 1 {
                bail!("oldText must not be empty in {path}.");
            }
            bail!("edits[{i}].oldText must not be empty in {path}.");
        }
    }

    let used_fuzzy = edits.iter().any(|e| matches!(fuzzy_find(content, &e.old_text), Some((_, _, true))));
    let base = if used_fuzzy { normalize_for_fuzzy_match(content) } else { content.to_string() };

    let mut matched = Vec::with_capacity(total);
    for (i, edit) in edits.iter().enumerate() {
        let Some((start, len, _)) = fuzzy_find(&base, &edit.old_text) else {
            if total == 1 {
                bail!(
                    "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
                );
            }
            bail!(
                "Could not find edits[{i}] in {path}. The oldText must match exactly including all whitespace and newlines."
            );
        };
        let occurrences = count_occurrences(&base, &edit.old_text);
        if occurrences > 1 {
            let what = edit_ref(path, i, total, "the text", "");
            let rule = if total == 1 { "The text must be unique" } else { "Each oldText must be unique" };
            bail!("Found {occurrences} occurrences of {what}. {rule}. Please provide more context to make it unique.");
        }
        matched.push(Replacement { edit_index: i, start, len, new_text: edit.new_text.clone() });
    }

    matched.sort_by_key(|r| r.start);
    for pair in matched.windows(2) {
        if pair[0].start + pair[0].len > pair[1].start {
            let (a, b) = (pair[0].edit_index, pair[1].edit_index);
            bail!("edits[{a}] and edits[{b}] overlap in {path}. Merge them into one edit or target disjoint regions.");
        }
    }

    let new_content = if used_fuzzy {
        apply_preserving_unchanged_lines(content, &base, &matched)?
    } else {
        apply_replacements(&base, &matched, 0)
    };
    if new_content == content {
        if total == 1 {
            bail!(
                "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
            );
        }
        bail!("No changes made to {path}. The replacements produced identical content.");
    }
    Ok((content.to_string(), new_content))
}
