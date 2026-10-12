//! Fuzzy matching: a query matches when all its characters appear in order in the text.
//! Lower scores are better. Positions are counted in UTF-16 code units to keep upstream scores.

use std::cmp::Ordering;

use crate::utils::{is_js_whitespace, js_trim};

/// Result of [`fuzzy_match`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FuzzyMatch {
    /// True when every query character was found in order.
    pub matches: bool,
    /// Match quality; lower is better. Zero when there is no match.
    pub score: f64,
}

const NO_MATCH: FuzzyMatch = FuzzyMatch { matches: false, score: 0.0 };

fn is_boundary_unit(unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(|c| is_js_whitespace(c) || matches!(c, '-' | '_' | '.' | '/' | ':'))
}

fn match_query(query: &[u16], text: &[u16]) -> FuzzyMatch {
    if query.is_empty() {
        return FuzzyMatch { matches: true, score: 0.0 };
    }
    if query.len() > text.len() {
        return NO_MATCH;
    }

    let mut score = 0.0;
    // Starts at -1 like upstream, so a first match at index 0 also counts as consecutive.
    let mut last_match: isize = -1;
    let mut consecutive = 0u32;
    for &unit in query {
        let from = (last_match + 1) as usize;
        let Some(offset) = text[from..].iter().position(|&u| u == unit) else {
            return NO_MATCH;
        };
        let i = from + offset;
        let is_word_boundary = i == 0 || is_boundary_unit(text[i - 1]);

        if last_match == i as isize - 1 {
            // Reward consecutive matches.
            consecutive += 1;
            score -= f64::from(consecutive) * 5.0;
        } else {
            consecutive = 0;
            if last_match >= 0 {
                // Penalize gaps.
                score += (i as isize - last_match - 1) as f64 * 2.0;
            }
        }
        if is_word_boundary {
            score -= 10.0;
        }
        // Slight penalty for later matches.
        score += i as f64 * 0.1;
        last_match = i as isize;
    }

    if query == text {
        score -= 100.0;
    }
    FuzzyMatch { matches: true, score }
}

/// Splits `letters+digits` or `digits+letters` (ASCII lowercase) and returns the halves swapped.
fn swapped_alphanumeric(query: &str) -> Option<String> {
    let split = query.find(|c: char| c.is_ascii_digit() != query.starts_with(|d: char| d.is_ascii_digit()))?;
    let (head, tail) = query.split_at(split);
    let letters = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase());
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if (letters(head) && digits(tail)) || (digits(head) && letters(tail)) {
        Some(format!("{tail}{head}"))
    } else {
        None
    }
}

/// Matches `query` against `text` case-insensitively. Also tries `codex52` as `52codex` and the reverse.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower = query.to_lowercase();
    let text_lower: Vec<u16> = text.to_lowercase().encode_utf16().collect();
    let query_units: Vec<u16> = query_lower.encode_utf16().collect();

    let primary = match_query(&query_units, &text_lower);
    if primary.matches {
        return primary;
    }
    let Some(swapped) = swapped_alphanumeric(&query_lower) else {
        return primary;
    };
    let swapped_units: Vec<u16> = swapped.encode_utf16().collect();
    let swapped_match = match_query(&swapped_units, &text_lower);
    if !swapped_match.matches {
        return primary;
    }
    FuzzyMatch { matches: true, score: swapped_match.score + 5.0 }
}

/// Keeps items whose text matches every whitespace- or slash-separated query token, best first.
/// Items with equal scores keep their input order. A blank query returns `items` unchanged.
pub fn fuzzy_filter<T, F>(items: Vec<T>, query: &str, get_text: F) -> Vec<T>
where
    F: Fn(&T) -> &str,
{
    let trimmed = js_trim(query);
    let tokens: Vec<&str> =
        trimmed.split(|c: char| is_js_whitespace(c) || c == '/').filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
        return items;
    }

    let mut results: Vec<(T, f64)> = Vec::new();
    for item in items {
        let total = {
            let text = get_text(&item);
            tokens.iter().try_fold(0.0, |total, token| {
                let result = fuzzy_match(token, text);
                result.matches.then_some(total + result.score)
            })
        };
        if let Some(total) = total {
            results.push((item, total));
        }
    }
    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
    results.into_iter().map(|(item, _)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_letter_digit_queries() {
        assert_eq!(swapped_alphanumeric("codex52").as_deref(), Some("52codex"));
        assert_eq!(swapped_alphanumeric("52codex").as_deref(), Some("codex52"));
        assert_eq!(swapped_alphanumeric("codex"), None);
        assert_eq!(swapped_alphanumeric("a1b"), None);
        assert_eq!(swapped_alphanumeric("A1"), None);
    }

    #[test]
    fn scores_like_upstream() {
        assert_eq!(fuzzy_match("ab", "ab"), FuzzyMatch { matches: true, score: -10.0 - 5.0 - 10.0 + 0.1 - 100.0 });
        assert_eq!(fuzzy_match("e", "eox"), FuzzyMatch { matches: true, score: -15.0 });
        assert_eq!(fuzzy_match("fb", "foo-bar"), FuzzyMatch { matches: true, score: -5.0 - 10.0 + 6.0 - 10.0 + 0.4 });
    }
}
