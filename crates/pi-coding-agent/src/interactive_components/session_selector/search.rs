//! Search for the resume picker (upstream `session-selector-search.ts`): fuzzy tokens, quoted
//! phrases, `re:` regex, sort modes, and the named filter. The logic lives in
//! [`crate::session::search`], shared with the CLI; this module re-exports it for the selector.

pub use crate::session::search::{
    FuzzyMatch as MatchResult, NameFilter, ParsedSearchQuery, SearchToken, SortMode, ThreadedSession,
    filter_and_sort_sessions, has_session_name, match_session, parse_search_query, threaded_sessions,
};

#[cfg(test)]
mod tests {
    //! Port of upstream `session-selector-search.test.ts`, plus parser cases.

    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::session::{SessionInfo, parse_iso};

    fn session(id: &str, modified: &str, text: &str, name: Option<&str>) -> SessionInfo {
        let modified = parse_iso(modified).expect("timestamp");
        SessionInfo {
            path: PathBuf::from(format!("/tmp/{id}.jsonl")),
            id: id.to_string(),
            cwd: String::new(),
            name: name.map(String::from),
            parent_session_path: None,
            created: 0,
            modified,
            changed: UNIX_EPOCH + Duration::from_millis(modified as u64),
            message_count: 1,
            first_message: "(no messages)".into(),
            all_messages_text: text.to_string(),
        }
    }

    fn ids(sessions: &[SessionInfo], query: &str, sort: SortMode, filter: NameFilter) -> Vec<String> {
        filter_and_sort_sessions(sessions, query, sort, filter).into_iter().map(|session| session.id).collect()
    }

    #[test]
    fn filters_by_quoted_phrase_with_whitespace_normalization() {
        let sessions = [
            session("a", "2026-01-01T00:00:00.000Z", "node\n\n   cve was discussed", None),
            session("b", "2026-01-02T00:00:00.000Z", "node something else", None),
        ];
        assert_eq!(ids(&sessions, "\"node cve\"", SortMode::Recent, NameFilter::All), ["a"]);
    }

    #[test]
    fn filters_by_regex_case_insensitively() {
        let sessions = [
            session("a", "2026-01-02T00:00:00.000Z", "Brave is great", None),
            session("b", "2026-01-03T00:00:00.000Z", "bravery is not the same", None),
        ];
        assert_eq!(ids(&sessions, "re:\\bbrave\\b", SortMode::Recent, NameFilter::All), ["a"]);
    }

    #[test]
    fn recent_sort_preserves_input_order() {
        let sessions = [
            session("newer", "2026-01-03T00:00:00.000Z", "brave", None),
            session("older", "2026-01-01T00:00:00.000Z", "brave", None),
            session("nomatch", "2026-01-04T00:00:00.000Z", "something else", None),
        ];
        assert_eq!(ids(&sessions, "\"brave\"", SortMode::Recent, NameFilter::All), ["newer", "older"]);
    }

    #[test]
    fn relevance_sort_orders_by_score_and_tie_breaks_by_modified_desc() {
        let sessions = [
            session("late", "2026-01-03T00:00:00.000Z", "xxxx brave", None),
            session("early", "2026-01-01T00:00:00.000Z", "brave xxxx", None),
        ];
        assert_eq!(ids(&sessions, "\"brave\"", SortMode::Relevance, NameFilter::All), ["early", "late"]);
        let ties = [
            session("older", "2026-01-01T00:00:00.000Z", "brave", None),
            session("newer", "2026-01-03T00:00:00.000Z", "brave", None),
        ];
        assert_eq!(ids(&ties, "\"brave\"", SortMode::Relevance, NameFilter::All), ["newer", "older"]);
    }

    #[test]
    fn returns_empty_list_for_invalid_regex() {
        let sessions = [session("a", "2026-01-01T00:00:00.000Z", "brave", None)];
        assert!(ids(&sessions, "re:(", SortMode::Recent, NameFilter::All).is_empty());
    }

    fn named_sessions() -> Vec<SessionInfo> {
        vec![
            session("named1", "2026-01-03T00:00:00.000Z", "blueberry", Some("My Project")),
            session("named2", "2026-01-02T00:00:00.000Z", "blueberry", Some("Another Named")),
            session("other1", "2026-01-04T00:00:00.000Z", "blueberry", None),
            session("other2", "2026-01-01T00:00:00.000Z", "blueberry", None),
        ]
    }

    #[test]
    fn name_filter_all_returns_every_session() {
        let all = ids(&named_sessions(), "", SortMode::Recent, NameFilter::All);
        assert_eq!(all, ["named1", "named2", "other1", "other2"]);
    }

    #[test]
    fn name_filter_named_returns_only_named_sessions() {
        assert_eq!(ids(&named_sessions(), "", SortMode::Recent, NameFilter::Named), ["named1", "named2"]);
    }

    #[test]
    fn name_filter_applies_before_the_search_query() {
        assert_eq!(ids(&named_sessions(), "blueberry", SortMode::Recent, NameFilter::Named), ["named1", "named2"]);
    }

    #[test]
    fn named_filter_excludes_whitespace_only_names() {
        let sessions = [
            session("whitespace", "2026-01-01T00:00:00.000Z", "test", Some("   ")),
            session("empty", "2026-01-02T00:00:00.000Z", "test", Some("")),
            session("named", "2026-01-03T00:00:00.000Z", "test", Some("Real Name")),
        ];
        assert_eq!(ids(&sessions, "", SortMode::Recent, NameFilter::Named), ["named"]);
    }

    #[test]
    fn parser_handles_phrases_regex_and_unbalanced_quotes() {
        let tokens = |query: &str| match parse_search_query(query) {
            ParsedSearchQuery::Tokens(tokens) => tokens,
            other => panic!("expected tokens, got {other:?}"),
        };
        let fuzzy = |value: &str| SearchToken::Fuzzy(value.to_string());
        let phrase = |value: &str| SearchToken::Phrase(value.to_string());
        assert_eq!(tokens("foo \"node cve\" bar"), [fuzzy("foo"), phrase("node cve"), fuzzy("bar")]);
        assert_eq!(tokens("  "), []);
        assert_eq!(tokens("foo \"node cve"), [fuzzy("foo"), fuzzy("\"node"), fuzzy("cve")]);
        assert!(matches!(parse_search_query("re:  "), ParsedSearchQuery::Invalid(message) if message == "Empty regex"));
        assert!(matches!(parse_search_query("re:ab+c"), ParsedSearchQuery::Regex(_)));
    }

    #[test]
    fn matching_scores_regex_position_and_fuzzy_tokens() {
        let target = session("id1", "2026-01-01T00:00:00.000Z", "hello brave world", None);
        let regex = match_session(&target, &parse_search_query("re:brave"));
        assert!(regex.matches);
        assert!((regex.score - "id1  hello ".len() as f64 * 0.1).abs() < 1e-9);
        assert!(match_session(&target, &parse_search_query("hlo wrld")).matches);
        assert!(!match_session(&target, &parse_search_query("zebra")).matches);
        let empty = match_session(&target, &parse_search_query(""));
        assert_eq!(empty, MatchResult { matches: true, score: 0.0 });
        assert!(has_session_name(&session("n", "2026-01-01T00:00:00.000Z", "", Some(" x "))));
    }
}
