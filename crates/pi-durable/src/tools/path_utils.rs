//! Path resolution for tool arguments: special spaces and a leading `@` that models add, and the spellings macOS
//! screenshot names use.

use pi_chord::Context;
use unicode_normalization::UnicodeNormalization;

use super::thrown;
use crate::env::ExecutionEnv;
use crate::errors::DurableResult;

const NARROW_NO_BREAK_SPACE: char = '\u{202F}';

fn is_unicode_space(c: char) -> bool {
    matches!(c, '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
}

fn normalize_tool_path(path: &str) -> String {
    let normalized: String = path.chars().map(|c| if is_unicode_space(c) { ' ' } else { c }).collect();
    normalized.strip_prefix('@').map(str::to_string).unwrap_or(normalized)
}

pub async fn resolve_tool_path(env: &dyn ExecutionEnv, path: &str, context: &Context) -> DurableResult<String> {
    env.absolute_path(&normalize_tool_path(path), context).await.map_err(thrown)
}

/// ` AM.` and ` PM.` (any case) with a narrow no-break space before them.
fn narrow_meridiem(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(index) = rest.find(' ') {
        result.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        let meridiem = after.get(..3).filter(|m| {
            let lower = m.to_ascii_lowercase();
            (lower == "am." || lower == "pm.") && m.is_ascii()
        });
        result.push(if meridiem.is_some() { NARROW_NO_BREAK_SPACE } else { ' ' });
        rest = after;
    }
    result.push_str(rest);
    result
}

/// The resolved path, or the first existing spelling of it: a narrow space before AM/PM, NFD, or a curly apostrophe.
pub async fn resolve_read_tool_path(env: &dyn ExecutionEnv, path: &str, context: &Context) -> DurableResult<String> {
    let resolved = resolve_tool_path(env, path, context).await?;
    let nfd: String = resolved.nfd().collect();
    let variants = [
        resolved.clone(),
        narrow_meridiem(&resolved),
        nfd.clone(),
        resolved.replace('\'', "\u{2019}"),
        nfd.replace('\'', "\u{2019}"),
    ];
    let mut seen: Vec<&String> = Vec::new();
    for variant in &variants {
        if seen.contains(&variant) {
            continue;
        }
        seen.push(variant);
        if env.exists(variant, context).await.map_err(thrown)? {
            return Ok(variant.clone());
        }
    }
    Ok(resolved)
}
