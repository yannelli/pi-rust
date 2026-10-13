//! Resume picker search: fuzzy tokens, quoted phrases, `re:` regex, sort modes, the named
//! filter, and the threaded (fork tree) order. Port of upstream `session-selector-search.ts`.

use std::collections::HashMap;
use std::path::PathBuf;

use regex::Regex;

use super::ids::resolve_path;
use super::listing::SessionInfo;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortMode {
    /// Forks under their parent session; a search query switches to relevance.
    #[default]
    Threaded,
    Recent,
    Relevance,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NameFilter {
    #[default]
    All,
    Named,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchToken {
    Fuzzy(String),
    Phrase(String),
}

#[derive(Clone, Debug)]
pub enum ParsedSearchQuery {
    Tokens(Vec<SearchToken>),
    Regex(Regex),
    /// The query could not be parsed and matches nothing.
    Invalid(String),
}

/// A fuzzy match result: lower scores are better.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

fn is_boundary(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '-' | '_' | '.' | '/' | ':')
}

fn match_query(query: &[char], text: &[char]) -> FuzzyMatch {
    let miss = FuzzyMatch { matches: false, score: 0.0 };
    if query.is_empty() {
        return FuzzyMatch { matches: true, score: 0.0 };
    }
    if query.len() > text.len() {
        return miss;
    }
    let (mut score, mut last, mut consecutive) = (0.0, None::<usize>, 0.0);
    for wanted in query {
        let start = last.map_or(0, |index| index + 1);
        let Some(offset) = text[start..].iter().position(|ch| ch == wanted) else { return miss };
        let index = start + offset;
        if last.is_some_and(|last| last + 1 == index) {
            consecutive += 1.0;
            score -= consecutive * 5.0;
        } else {
            consecutive = 0.0;
            if let Some(last) = last {
                score += (index - last - 1) as f64 * 2.0;
            }
        }
        if index == 0 || is_boundary(text[index - 1]) {
            score -= 10.0;
        }
        score += index as f64 * 0.1;
        last = Some(index);
    }
    if query == text {
        score -= 100.0;
    }
    FuzzyMatch { matches: true, score }
}

/// Characters of `query` in order within `text`, case-insensitive, as upstream `fuzzyMatch`.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query: Vec<char> = query.to_lowercase().chars().collect();
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let primary = match_query(&query, &text);
    if primary.matches {
        return primary;
    }
    // Retry letters+digits as digits+letters (and back), slightly penalized.
    let split = query.iter().position(|ch| ch.is_ascii_digit() != query[0].is_ascii_digit());
    let swapped: Option<Vec<char>> = split.and_then(|split| {
        let (head, tail) = query.split_at(split);
        let letters_then_digits = head.iter().all(char::is_ascii_lowercase) && tail.iter().all(char::is_ascii_digit);
        let digits_then_letters = head.iter().all(char::is_ascii_digit) && tail.iter().all(char::is_ascii_lowercase);
        (letters_then_digits || digits_then_letters).then(|| tail.iter().chain(head).copied().collect())
    });
    match swapped.map(|swapped| match_query(&swapped, &text)) {
        Some(found) if found.matches => FuzzyMatch { matches: true, score: found.score + 5.0 },
        _ => primary,
    }
}

/// Parse a picker query: `re:<pattern>`, or tokens with `"quoted phrases"`.
pub fn parse_search_query(query: &str) -> ParsedSearchQuery {
    let trimmed = query.trim();
    if let Some(pattern) = trimmed.strip_prefix("re:") {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return ParsedSearchQuery::Invalid("Empty regex".into());
        }
        return match Regex::new(&format!("(?i){pattern}")) {
            Ok(regex) => ParsedSearchQuery::Regex(regex),
            Err(error) => ParsedSearchQuery::Invalid(error.to_string()),
        };
    }
    let mut tokens = Vec::new();
    let (mut buffer, mut in_quote) = (String::new(), false);
    let flush = |buffer: &mut String, tokens: &mut Vec<SearchToken>, phrase: bool| {
        let value = buffer.trim().to_string();
        buffer.clear();
        if !value.is_empty() {
            tokens.push(if phrase { SearchToken::Phrase(value) } else { SearchToken::Fuzzy(value) });
        }
    };
    for ch in trimmed.chars() {
        if ch == '"' {
            flush(&mut buffer, &mut tokens, in_quote);
            in_quote = !in_quote;
        } else if !in_quote && ch.is_whitespace() {
            flush(&mut buffer, &mut tokens, false);
        } else {
            buffer.push(ch);
        }
    }
    if in_quote {
        let words = trimmed.split_whitespace().map(|word| SearchToken::Fuzzy(word.to_string()));
        return ParsedSearchQuery::Tokens(words.collect());
    }
    flush(&mut buffer, &mut tokens, false);
    ParsedSearchQuery::Tokens(tokens)
}

fn normalize_whitespace_lower(text: &str) -> String {
    text.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a session matches, and its score (lower is better).
pub fn match_session(session: &SessionInfo, query: &ParsedSearchQuery) -> FuzzyMatch {
    let miss = FuzzyMatch { matches: false, score: 0.0 };
    let text = format!(
        "{} {} {} {}",
        session.id,
        session.name.as_deref().unwrap_or_default(),
        session.all_messages_text,
        session.cwd
    );
    let tokens = match query {
        ParsedSearchQuery::Invalid(_) => return miss,
        ParsedSearchQuery::Regex(regex) => {
            return match regex.find(&text) {
                Some(found) => FuzzyMatch { matches: true, score: text[..found.start()].chars().count() as f64 * 0.1 },
                None => miss,
            };
        }
        ParsedSearchQuery::Tokens(tokens) => tokens,
    };
    let mut total = 0.0;
    let mut normalized: Option<String> = None;
    for token in tokens {
        match token {
            SearchToken::Phrase(phrase) => {
                let haystack = normalized.get_or_insert_with(|| normalize_whitespace_lower(&text));
                let phrase = normalize_whitespace_lower(phrase);
                if phrase.is_empty() {
                    continue;
                }
                let Some(index) = haystack.find(&phrase) else { return miss };
                total += haystack[..index].chars().count() as f64 * 0.1;
            }
            SearchToken::Fuzzy(value) => {
                let found = fuzzy_match(value, &text);
                if !found.matches {
                    return miss;
                }
                total += found.score;
            }
        }
    }
    FuzzyMatch { matches: true, score: total }
}

pub fn has_session_name(session: &SessionInfo) -> bool {
    session.name.as_deref().is_some_and(|name| !name.trim().is_empty())
}

/// Apply the named filter, then the query: `Recent` keeps input order, otherwise matches
/// sort by score with newer sessions first on ties.
pub fn filter_and_sort_sessions(
    sessions: &[SessionInfo],
    query: &str,
    sort: SortMode,
    filter: NameFilter,
) -> Vec<SessionInfo> {
    let named: Vec<&SessionInfo> =
        sessions.iter().filter(|session| filter == NameFilter::All || has_session_name(session)).collect();
    if query.trim().is_empty() {
        return named.into_iter().cloned().collect();
    }
    let parsed = parse_search_query(query);
    if matches!(parsed, ParsedSearchQuery::Invalid(_)) {
        return Vec::new();
    }
    let mut scored: Vec<(&SessionInfo, f64)> = named
        .into_iter()
        .filter_map(|session| {
            let found = match_session(session, &parsed);
            found.matches.then_some((session, found.score))
        })
        .collect();
    if sort != SortMode::Recent {
        scored.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.modified.cmp(&a.0.modified)));
    }
    scored.into_iter().map(|(session, _)| session.clone()).collect()
}

/// One row of the threaded order: a session, its depth, and tree-drawing hints.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadedSession {
    pub session: SessionInfo,
    pub depth: usize,
    pub is_last: bool,
    /// For each ancestor level, whether more siblings follow it.
    pub ancestor_continues: Vec<bool>,
}

fn canonical(path: &str) -> PathBuf {
    let path = resolve_path(std::path::Path::new(path));
    std::fs::canonicalize(&path).unwrap_or(path)
}

/// Sessions as a fork tree (children under `parent_session_path`), subtrees ordered by their
/// latest activity, flattened in display order.
pub fn threaded_sessions(sessions: &[SessionInfo]) -> Vec<ThreadedSession> {
    let paths: Vec<PathBuf> = sessions.iter().map(|session| canonical(&session.path.display().to_string())).collect();
    let by_path: HashMap<&PathBuf, usize> = paths.iter().enumerate().map(|(index, path)| (path, index)).collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); sessions.len()];
    let mut roots = Vec::new();
    for (index, session) in sessions.iter().enumerate() {
        let parent = session.parent_session_path.as_deref().map(canonical);
        match parent.as_ref().and_then(|parent| by_path.get(parent)).filter(|parent| **parent != index) {
            Some(parent) => children[*parent].push(index),
            None => roots.push(index),
        }
    }
    let mut latest: Vec<i64> = sessions.iter().map(|session| session.modified).collect();
    // Children always follow parents in a post-order walk built with an explicit stack.
    let mut order = Vec::new();
    let mut stack: Vec<(usize, bool)> = roots.iter().map(|root| (*root, false)).collect();
    while let Some((node, expanded)) = stack.pop() {
        if expanded {
            order.push(node);
        } else {
            stack.push((node, true));
            stack.extend(children[node].iter().map(|child| (*child, false)));
        }
    }
    for node in order {
        let newest = children[node].iter().map(|child| latest[*child]).max().unwrap_or(i64::MIN);
        latest[node] = latest[node].max(newest);
    }
    let by_latest = |list: &mut Vec<usize>| list.sort_by(|a, b| latest[*b].cmp(&latest[*a]));
    by_latest(&mut roots);
    for list in &mut children {
        by_latest(list);
    }
    let mut rows = Vec::new();
    let mut stack: Vec<(usize, usize, Vec<bool>, bool)> = Vec::new();
    for (position, root) in roots.iter().enumerate().rev() {
        stack.push((*root, 0, Vec::new(), position == roots.len() - 1));
    }
    while let Some((node, depth, ancestors, is_last)) = stack.pop() {
        let count = children[node].len();
        for (position, child) in children[node].iter().enumerate().rev() {
            let mut next = ancestors.clone();
            next.push(depth > 0 && !is_last);
            stack.push((*child, depth + 1, next, position == count - 1));
        }
        rows.push(ThreadedSession { session: sessions[node].clone(), depth, is_last, ancestor_continues: ancestors });
    }
    rows
}
