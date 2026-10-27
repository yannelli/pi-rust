//! Resume picker search: fuzzy tokens, quoted phrases, `re:` regex, sort modes, the named
//! filter, and the threaded (fork tree) order. Port of upstream `session-selector-search.ts`.

use std::cell::OnceCell;
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

pub use pi_tui::fuzzy::FuzzyMatch;
use pi_tui::fuzzy::fuzzy_match;

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

/// Match tiers, best first. Each token scores by the best field it matches; tiers dominate
/// fuzzy scores, so an exact hit anywhere beats a scattered subsequence in a short field.
const TIER_NAME: f64 = 0.0; // query text appears in the session name
const TIER_TEXT: f64 = 1.0; // query text appears in the session id, messages, or cwd
const TIER_FUZZY: f64 = 2.0; // query characters appear scattered in the session name or cwd
const TIER_WEIGHT: f64 = 1_000_000.0;

fn tier_score(tier: f64, score: f64) -> f64 {
    tier * TIER_WEIGHT + score
}

/// Lowercased fields of one session, computed once per match. The message text can be
/// megabytes, so it is lowercased only when a token needs it.
struct SessionFields<'a> {
    session: &'a SessionInfo,
    messages: OnceCell<String>,
    normalized_messages: OnceCell<String>,
}

impl SessionFields<'_> {
    fn messages(&self) -> &str {
        self.messages.get_or_init(|| self.session.all_messages_text.to_lowercase())
    }

    fn normalized_messages(&self) -> &str {
        self.normalized_messages.get_or_init(|| self.messages().split_whitespace().collect::<Vec<_>>().join(" "))
    }

    /// Exact (case-insensitive) text match. Messages and ids only match exactly: scattered
    /// matches in them are noise.
    fn literal(&self, value: &str) -> Option<f64> {
        let session = self.session;
        let name = session.name.as_deref().unwrap_or_default().to_lowercase();
        if name.contains(value) {
            return Some(tier_score(TIER_NAME, fuzzy_match(value, &name).score));
        }
        let text = session.id.to_lowercase().contains(value)
            || self.messages().contains(value)
            || session.cwd.to_lowercase().contains(value);
        text.then_some(tier_score(TIER_TEXT, 0.0))
    }

    fn fuzzy(&self, value: &str) -> Option<f64> {
        if let Some(score) = self.literal(&value.to_lowercase()) {
            return Some(score);
        }
        let fields = [self.session.name.as_deref().unwrap_or_default(), self.session.cwd.as_str()];
        let best = fields
            .into_iter()
            .map(|field| fuzzy_match(value, field))
            .filter(|found| found.matches)
            .map(|found| found.score)
            .reduce(f64::min)?;
        Some(tier_score(TIER_FUZZY, best))
    }

    fn phrase(&self, phrase: &str) -> Option<f64> {
        let session = self.session;
        let name = normalize_whitespace_lower(session.name.as_deref().unwrap_or_default());
        if name.contains(phrase) {
            return Some(tier_score(TIER_NAME, fuzzy_match(phrase, &name).score));
        }
        let text = session.id.to_lowercase().contains(phrase)
            || self.normalized_messages().contains(phrase)
            || normalize_whitespace_lower(&session.cwd).contains(phrase);
        text.then_some(tier_score(TIER_TEXT, 0.0))
    }

    fn regex(&self, regex: &Regex) -> Option<f64> {
        let session = self.session;
        if session.name.as_deref().is_some_and(|name| !name.is_empty() && regex.is_match(name)) {
            return Some(tier_score(TIER_NAME, 0.0));
        }
        let text =
            regex.is_match(&session.id) || regex.is_match(&session.all_messages_text) || regex.is_match(&session.cwd);
        text.then_some(tier_score(TIER_TEXT, 0.0))
    }
}

/// Whether a session matches, and its score (lower is better). Each field is matched
/// separately, so a token never matches across field boundaries; different tokens may match
/// different fields.
pub fn match_session(session: &SessionInfo, query: &ParsedSearchQuery) -> FuzzyMatch {
    let miss = FuzzyMatch { matches: false, score: 0.0 };
    let fields = SessionFields { session, messages: OnceCell::new(), normalized_messages: OnceCell::new() };
    let tokens = match query {
        ParsedSearchQuery::Invalid(_) => return miss,
        ParsedSearchQuery::Regex(regex) => {
            return fields.regex(regex).map_or(miss, |score| FuzzyMatch { matches: true, score });
        }
        ParsedSearchQuery::Tokens(tokens) => tokens,
    };
    let mut total = 0.0;
    for token in tokens {
        let score = match token {
            SearchToken::Phrase(phrase) => {
                let phrase = normalize_whitespace_lower(phrase);
                if phrase.is_empty() {
                    continue;
                }
                fields.phrase(&phrase)
            }
            SearchToken::Fuzzy(value) => fields.fuzzy(value),
        };
        let Some(score) = score else { return miss };
        total += score;
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
