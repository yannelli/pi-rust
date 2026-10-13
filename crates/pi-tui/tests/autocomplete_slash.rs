//! Port of upstream `autocomplete-skill-slash.test.ts`, plus provider wrapping.

mod autocomplete_common;

use std::rc::Rc;

use autocomplete_common::{apply, suggest, values};
use pi_tui::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteProviderFactory, CombinedAutocompleteProvider, CommandEntry,
    SlashCommand, WithTriggerCharacters, wrap_autocomplete_provider,
};

fn commands() -> Vec<CommandEntry> {
    [
        ("skill:deep-research", "Multi-agent deep research"),
        ("skill:research-idea", "Refine a raw idea into a falsifiable seed"),
        ("skill:to-sidecar", "Route work to a sidecar"),
        ("skill:brainstorm", "Generate ideas"),
        ("model", "Select the active model"),
    ]
    .into_iter()
    .map(|(name, description)| SlashCommand::new(name, Some(description)).into())
    .collect()
}

fn suggestions_for(prefix: &str) -> Vec<String> {
    let provider = CombinedAutocompleteProvider::new(commands(), ".", None);
    let line = format!("/{prefix}");
    let result = suggest(&provider, &line, line.len(), false);
    assert!(result.is_some(), "expected suggestions for {line:?}");
    values(&result)
}

#[test]
fn ranks_research_idea_first_for_idea() {
    let items = suggestions_for("idea");
    assert_eq!(items[0], "skill:research-idea");
    let position = |name: &str| items.iter().position(|item| item == name);
    assert!(position("skill:deep-research") > position("skill:research-idea"));
}

#[test]
fn keeps_ordinary_commands_matching() {
    assert!(suggestions_for("mod").contains(&"model".to_string()));
}

#[test]
fn completes_commands_after_leading_whitespace() {
    let provider = CombinedAutocompleteProvider::new(vec![SlashCommand::new("model", None).into()], ".", None);
    for (line, expected) in [(" /", " /model "), ("  /mod", "  /model "), ("\t/mod", "\t/model ")] {
        let result = suggest(&provider, line, line.len(), false).expect("suggestions");
        assert_eq!(result.prefix, line.trim_start());
        assert_eq!(values(&Some(result.clone())), ["model"]);
        let applied = apply(&provider, line, line.len(), &result.items[0], &result.prefix);
        assert_eq!(applied.lines[0], expected);
        assert_eq!(applied.cursor_col, expected.len());
    }
}

#[test]
fn completes_arguments_after_leading_whitespace() {
    let command = SlashCommand::new("model", None).with_argument_completions(|prefix| {
        assert_eq!(prefix, "son");
        Some(vec![AutocompleteItem::new("sonnet", "sonnet")])
    });
    let provider = CombinedAutocompleteProvider::new(vec![command.into()], ".", None);
    let line = "  /model son";
    let result = suggest(&provider, line, line.len(), false).expect("suggestions");
    assert_eq!(result.prefix, "son");
    assert_eq!(apply(&provider, line, line.len(), &result.items[0], &result.prefix).lines[0], "  /model sonnet");
}

#[test]
fn keeps_explicit_skill_queries_working() {
    assert!(suggestions_for("skill:side").contains(&"skill:to-sidecar".to_string()));
}

#[test]
fn lists_skills_while_typing_the_skill_prefix() {
    let skills: Vec<String> = suggestions_for("skill").into_iter().filter(|item| item.starts_with("skill:")).collect();
    let expected: Vec<String> =
        commands().iter().map(|c| c.name().to_string()).filter(|name| name.starts_with("skill:")).collect();
    assert_eq!(skills, expected);
}

#[test]
fn keeps_fuzzy_skill_shorthand_working() {
    assert!(suggestions_for("skbra").contains(&"skill:brainstorm".to_string()));
}

#[test]
fn describes_commands_with_argument_hints() {
    let commands = vec![
        SlashCommand::new("model", Some("Switch model")).with_argument_hint("<id>").into(),
        SlashCommand::new("login", None).with_argument_hint("<provider>").into(),
        CommandEntry::Item(AutocompleteItem::new("plain", "plain").with_description("Plain item")),
    ];
    let provider = CombinedAutocompleteProvider::new(commands, ".", None);
    let result = suggest(&provider, "/", 1, false).expect("suggestions");
    let descriptions: Vec<Option<String>> = result.items.iter().map(|item| item.description.clone()).collect();
    assert_eq!(
        descriptions,
        [Some("<id> \u{2014} Switch model".into()), Some("<provider>".into()), Some("Plain item".into())]
    );
}

#[test]
fn file_completion_skips_command_names() {
    let provider = CombinedAutocompleteProvider::new(commands(), ".", None);
    let lines = vec!["/mod".to_string()];
    assert!(!provider.should_trigger_file_completion(&lines, 0, 4));
    let lines = vec!["/model ./".to_string()];
    assert!(provider.should_trigger_file_completion(&lines, 0, 9));
}

#[test]
fn wrapping_merges_trigger_characters() {
    let base: Rc<dyn AutocompleteProvider> = Rc::new(CombinedAutocompleteProvider::new(commands(), ".", None));
    let dollar: AutocompleteProviderFactory =
        Rc::new(|inner| Rc::new(WithTriggerCharacters { inner, triggers: vec!["$".into()] }));
    let percent: AutocompleteProviderFactory =
        Rc::new(|inner| Rc::new(WithTriggerCharacters { inner, triggers: vec!["%".into(), "$".into()] }));
    let wrapped = wrap_autocomplete_provider(base.clone(), &[dollar, percent]);
    assert_eq!(wrapped.trigger_characters(), ["$", "%"]);
    assert!(wrap_autocomplete_provider(base, &[]).trigger_characters().is_empty());
}
