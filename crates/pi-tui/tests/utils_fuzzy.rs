//! Ported from upstream test/fuzzy.test.ts.

use pi_tui::fuzzy::{fuzzy_filter, fuzzy_match};

#[test]
fn empty_query_matches_everything_with_score_0() {
    let result = fuzzy_match("", "anything");
    assert!(result.matches);
    assert_eq!(result.score, 0.0);
}

#[test]
fn query_longer_than_text_does_not_match() {
    assert!(!fuzzy_match("longquery", "short").matches);
}

#[test]
fn exact_match_has_good_score() {
    let result = fuzzy_match("test", "test");
    assert!(result.matches);
    assert!(result.score < 0.0);
}

#[test]
fn characters_must_appear_in_order() {
    assert!(fuzzy_match("abc", "aXbXc").matches);
    assert!(!fuzzy_match("abc", "cba").matches);
}

#[test]
fn case_insensitive_matching() {
    assert!(fuzzy_match("ABC", "abc").matches);
    assert!(fuzzy_match("abc", "ABC").matches);
}

#[test]
fn consecutive_matches_score_better_than_scattered_matches() {
    let consecutive = fuzzy_match("foo", "foobar");
    let scattered = fuzzy_match("foo", "f_o_o_bar");
    assert!(consecutive.matches);
    assert!(scattered.matches);
    assert!(consecutive.score < scattered.score);
}

#[test]
fn word_boundary_matches_score_better() {
    let at_boundary = fuzzy_match("fb", "foo-bar");
    let not_at_boundary = fuzzy_match("fb", "afbx");
    assert!(at_boundary.matches);
    assert!(not_at_boundary.matches);
    assert!(at_boundary.score < not_at_boundary.score);
}

// Upstream #11220: greedy alignment took o, p, u from "openrouter", so the real "opus" scored worst.
#[test]
fn scores_a_contiguous_occurrence_instead_of_an_earlier_scattered_alignment() {
    let opus = fuzzy_match("opus", "openrouter openrouter/anthropic/claude-opus-4.1");
    let sonnet = fuzzy_match("opus", "openrouter openrouter/anthropic/claude-sonnet-4.5");
    let gpt_oss = fuzzy_match("opus", "openrouter openrouter/openai/gpt-oss-120b");
    assert!(opus.score < sonnet.score);
    assert!(opus.score < gpt_oss.score);
}

#[test]
fn uses_the_best_contiguous_occurrence_not_the_first() {
    let embedded_first = fuzzy_match("opus", "xxopus opus");
    let boundary_only = fuzzy_match("opus", "xxxxxx opus");
    assert_eq!(embedded_first.score, boundary_only.score);
}

#[test]
fn matches_swapped_alpha_numeric_tokens() {
    assert!(fuzzy_match("codex52", "gpt-5.2-codex").matches);
}

#[test]
fn empty_query_returns_all_items_unchanged() {
    let items = vec!["apple", "banana", "cherry"];
    assert_eq!(fuzzy_filter(items.clone(), "", |x| x), items);
}

#[test]
fn filters_out_non_matching_items() {
    let result = fuzzy_filter(vec!["apple", "banana", "cherry"], "an", |x| x);
    assert!(result.contains(&"banana"));
    assert!(!result.contains(&"apple"));
    assert!(!result.contains(&"cherry"));
}

#[test]
fn sorts_results_by_match_quality() {
    let result = fuzzy_filter(vec!["a_p_p", "app", "application"], "app", |x| x);
    assert_eq!(result[0], "app");
}

#[test]
fn prioritizes_exact_matches_over_longer_prefix_matches() {
    assert_eq!(fuzzy_filter(vec!["clone", "cl"], "cl", |x| x), ["cl", "clone"]);
}

#[derive(Clone, Debug, PartialEq)]
struct Named {
    name: String,
    id: u32,
}

#[test]
fn works_with_custom_get_text_function() {
    let items = vec![
        Named { name: "foo".into(), id: 1 },
        Named { name: "bar".into(), id: 2 },
        Named { name: "foobar".into(), id: 3 },
    ];
    let result = fuzzy_filter(items, "foo", |item: &Named| item.name.as_str());
    assert_eq!(result.len(), 2);
    let names: Vec<&str> = result.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"foo"));
    assert!(names.contains(&"foobar"));
    assert!(result.iter().all(|r| r.id != 2));
}

#[test]
fn matches_slash_separated_provider_model_queries_against_reordered_text() {
    #[derive(Debug, PartialEq)]
    struct Model {
        id: &'static str,
        provider: &'static str,
    }
    let model = Model { id: "gpt-5.5", provider: "openai-codex" };
    // The text accessor borrows, so the joined text is built next to each item.
    let text = format!("{} {}", model.id, model.provider);
    let result = fuzzy_filter(vec![(&model, text)], "openai-codex/gpt-5.5", |(_, text)| text.as_str());
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, &model);
}

// Upstream #11220: "radius radius/gpt" contains a, s, t, r, a in order, so every radius model matched there.
#[test]
fn ranks_a_model_containing_the_query_above_models_matching_a_shared_provider_prefix() {
    let items = vec![
        "radius radius/gpt-5.4 radius gpt-5.4 GPT 5.4",
        "radius radius/agentic radius agentic Agentic",
        "radius radius/gpt-6-sol radius gpt-6-sol GPT 6 Sol",
        "radius radius/gpt-6-astra radius gpt-6-astra GPT 6 Astra",
    ];
    let result = fuzzy_filter(items.clone(), "astra", |item: &&str| item);
    assert_eq!(result[0], items[3]);
}
