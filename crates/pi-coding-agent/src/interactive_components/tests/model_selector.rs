//! Ports `model-selector.test.ts` and regression 7209 (filter resets the selection).

use std::cell::RefCell;
use std::rc::Rc;

use pi_ai::Model;
use pi_tui::Component;

use crate::interactive_components::model_selector::{
    DefaultModelReference, ModelSelector, ModelSelectorOptions, ModelSelectorSource, ModelsRefreshResult,
};
use crate::interactive_components::test_support::{Calls, Harness, flush, keys, local, plain, setup, setup_with};
use crate::model_resolver::ScopedModel;

struct FakeSource {
    models: RefCell<Vec<Model>>,
    error: Option<String>,
}

impl ModelSelectorSource for FakeSource {
    fn available_models(&self) -> Vec<Model> {
        self.models.borrow().clone()
    }

    fn find_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.models.borrow().iter().find(|model| model.provider == provider && model.id == id).cloned()
    }

    fn catalog_error(&self) -> Option<String> {
        self.error.clone()
    }
}

fn model(provider: &str, id: &str, name: &str) -> Model {
    Model { id: id.into(), name: name.into(), provider: provider.into(), reasoning: true, ..Default::default() }
}

fn source(models: Vec<Model>) -> Rc<FakeSource> {
    Rc::new(FakeSource { models: RefCell::new(models), error: None })
}

fn rendered(selector: &Rc<RefCell<ModelSelector>>) -> String {
    plain(&selector.borrow_mut().render(120)).join("\n")
}

fn selected_model_id(rendered: &str) -> Option<String> {
    let line = rendered.lines().find(|line| line.starts_with("→ "))?;
    let rest = line.trim_start_matches("→").trim_start();
    let id = rest.trim_start_matches("✓").trim().split(' ').next()?;
    (!id.is_empty()).then(|| id.to_string())
}

fn refreshed() -> crate::interactive_components::model_selector::ModelRefreshFn {
    Box::new(|_| Box::pin(async { Ok(ModelsRefreshResult::default()) }))
}

#[test]
fn keeps_the_current_model_marked_while_browsing() {
    let _kb = setup();
    let models = vec![model("faux", "current-model", "Current Model"), model("faux", "browsed-model", "Browsed Model")];
    let harness = Harness::new_shared(120, 30, |tui| {
        ModelSelector::new(
            tui,
            Some(models[0].clone()),
            source(models.clone()),
            Vec::new(),
            |_| {},
            || {},
            Default::default(),
        )
    });
    let row = |id: &str| {
        harness
            .render(120)
            .into_iter()
            .find(|line| line.contains(&format!("{id} [")))
            .map(|line| line.trim_end().to_string())
    };
    assert_eq!(row("current-model").as_deref(), Some("→ ✓ current-model [faux]"));
    harness.send(keys::DOWN);
    assert_eq!(row("current-model").as_deref(), Some("  ✓ current-model [faux]"));
    assert_eq!(row("browsed-model").as_deref(), Some("→   browsed-model [faux]"));
    let screen = harness.screen();
    assert!(screen.contains("Only showing models from configured providers. Use /login to add providers."));
    assert!(screen.contains("Model Name: Browsed Model"), "{screen}");
}

#[test]
fn moves_by_one_page_with_remapped_selection_page_keys() {
    let _kb = setup_with(&[("tui.select.pageUp", "alt+v"), ("tui.select.pageDown", "ctrl+v")]);
    let models: Vec<Model> = (0..12).map(|i| model("faux", &format!("model-{i:02}"), "")).collect();
    let selected = Calls::new();
    let on_select = selected.recorder();
    let harness = Harness::new_shared(120, 30, |tui| {
        let mut on_select = on_select;
        ModelSelector::new(
            tui,
            None,
            source(models.clone()),
            Vec::new(),
            move |model: &Model| on_select(model.id.clone()),
            || {},
            Default::default(),
        )
    });
    assert!(harness.component.borrow().captures_viewport_input());
    harness.send_all(&["\x16", "\x16", "\x1bv", keys::ENTER]);
    assert_eq!(selected.get(), ["model-01"]);
}

#[test]
fn uses_the_configured_save_binding() {
    let _kb = setup_with(&[("app.models.save", "ctrl+r")]);
    let current = model("faux", "m1", "M1");
    let saved = Calls::new();
    let calls = saved.clone();
    let options = ModelSelectorOptions {
        on_select_as_default: Some(Box::new(move |model: &Model| calls.push(model.id.clone()))),
        ..Default::default()
    };
    let harness = Harness::new_shared(120, 30, |tui| {
        ModelSelector::new(
            tui,
            Some(current.clone()),
            source(vec![current.clone()]),
            Vec::new(),
            |_| {},
            || {},
            options,
        )
    });
    assert!(harness.screen().contains("Ctrl+R to set as default"));
    harness.send("\x13");
    assert!(saved.is_empty());
    harness.send("\x12");
    assert_eq!(saved.get(), ["m1"]);
}

#[tokio::test(flavor = "current_thread")]
async fn lists_every_catalog_that_failed_to_refresh() {
    local(async {
        let _kb = setup();
        let current = model("openai", "gpt", "GPT");
        let refresh: crate::interactive_components::model_selector::ModelRefreshFn = Box::new(|_| {
            Box::pin(async {
                Ok(ModelsRefreshResult { aborted: false, errors: vec!["openai".into(), "anthropic".into()] })
            })
        });
        let options = ModelSelectorOptions { refresh: Some(refresh), ..Default::default() };
        let harness = Harness::new_shared(120, 30, |tui| {
            ModelSelector::new(
                tui,
                Some(current.clone()),
                source(vec![current.clone()]),
                Vec::new(),
                |_| {},
                || {},
                options,
            )
        });
        assert!(harness.screen().contains("Refreshing model catalogs…"));
        flush(&harness).await;
        let text = rendered(&harness.component);
        assert!(
            text.contains("Could not refresh 2 model catalogs (openai, anthropic); showing cached models."),
            "{text}"
        );
        assert!(!text.contains("Refreshing"));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn refresh_reloads_the_snapshot_and_reports_single_failures() {
    local(async {
        let _kb = setup();
        let fake = source(vec![model("faux", "a", "A")]);
        let late = fake.clone();
        let refresh: crate::interactive_components::model_selector::ModelRefreshFn = Box::new(move |_| {
            Box::pin(async move {
                late.models.borrow_mut().push(model("faux", "b", "B"));
                Ok(ModelsRefreshResult::default())
            })
        });
        let options = ModelSelectorOptions { refresh: Some(refresh), ..Default::default() };
        let harness = Harness::new_shared(120, 30, |tui| {
            ModelSelector::new(tui, None, fake.clone(), Vec::new(), |_| {}, || {}, options)
        });
        flush(&harness).await;
        let screen = harness.screen();
        assert!(screen.contains("Model catalogs refreshed.") && screen.contains("  b [faux]"), "{screen}");

        let failing: crate::interactive_components::model_selector::ModelRefreshFn =
            Box::new(|_| Box::pin(async { Ok(ModelsRefreshResult { aborted: false, errors: vec!["radius".into()] }) }));
        let options = ModelSelectorOptions { refresh: Some(failing), ..Default::default() };
        let harness = Harness::new_shared(120, 30, |tui| {
            ModelSelector::new(tui, None, fake.clone(), Vec::new(), |_| {}, || {}, options)
        });
        flush(&harness).await;
        assert!(harness.screen().contains("Could not refresh radius; showing cached models."));
    })
    .await;
}

fn alpha_models() -> Vec<Model> {
    vec![
        model("faux", "alpha-1", "Alpha 1"),
        model("faux", "alpha-2", "Alpha 2"),
        model("faux", "alpha-3", "Alpha 3"),
        model("faux", "beta-1", "Beta 1"),
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn typing_a_query_moves_the_selection_to_the_first_row_in_all() {
    local(async {
        let _kb = setup();
        let models = alpha_models();
        let options = ModelSelectorOptions { refresh: Some(refreshed()), ..Default::default() };
        let harness = Harness::new_shared(120, 30, |tui| {
            ModelSelector::new(tui, Some(models[0].clone()), source(models.clone()), Vec::new(), |_| {}, || {}, options)
        });
        flush(&harness).await;
        assert!(rendered(&harness.component).contains("Model catalogs refreshed."));
        assert_eq!(selected_model_id(&rendered(&harness.component)).as_deref(), Some("alpha-1"));
        harness.send_all(&[keys::DOWN, keys::DOWN]);
        assert_eq!(selected_model_id(&rendered(&harness.component)).as_deref(), Some("alpha-3"));
        harness.type_text("alpha");
        let text = rendered(&harness.component);
        assert_eq!(selected_model_id(&text).as_deref(), Some("alpha-1"));
        assert!(!text.contains("beta-1"));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn typing_a_query_moves_the_selection_to_the_first_row_in_scoped() {
    local(async {
        let _kb = setup();
        let models = alpha_models();
        let scoped: Vec<ScopedModel> =
            [1, 2, 0].iter().map(|&index| ScopedModel { model: models[index].clone(), thinking_level: None }).collect();
        let options = ModelSelectorOptions { refresh: Some(refreshed()), ..Default::default() };
        let harness = Harness::new_shared(120, 30, |tui| {
            ModelSelector::new(tui, Some(models[0].clone()), source(models.clone()), scoped, |_| {}, || {}, options)
        });
        flush(&harness).await;
        let text = rendered(&harness.component);
        assert!(text.contains("Model catalogs refreshed."));
        assert!(text.contains("Scope: all | scoped"));
        assert_eq!(selected_model_id(&text).as_deref(), Some("alpha-1"));
        harness.type_text("alpha");
        assert_eq!(selected_model_id(&rendered(&harness.component)).as_deref(), Some("alpha-2"));
    })
    .await;
}

#[test]
fn tab_switches_scope_and_default_model_sorts_and_searches() {
    let models = alpha_models();
    let scoped = vec![ScopedModel { model: models[3].clone(), thinking_level: None }];
    let selected = Calls::new();
    let on_select = selected.recorder();
    let options = ModelSelectorOptions {
        default_model: Some(DefaultModelReference { provider: "faux".into(), id: "alpha-3".into() }),
        ..Default::default()
    };
    let harness = Harness::new_shared(120, 30, |tui| {
        let mut on_select = on_select;
        ModelSelector::new(
            tui,
            Some(models[1].clone()),
            source(models.clone()),
            scoped,
            move |model: &Model| on_select(model.id.clone()),
            || {},
            options,
        )
    });
    let screen = harness.screen();
    assert!(screen.contains("→   beta-1 [faux]"), "{screen}");
    assert!(screen.contains("tab scope (all/scoped)"));
    harness.send(keys::TAB);
    let lines = harness.render(120);
    let ids: Vec<String> = lines
        .iter()
        .map(|line| line.chars().skip(4).collect::<String>())
        .filter(|rest| rest.contains(" [faux]"))
        .collect();
    assert!(ids[0].starts_with("alpha-2") && ids[1].starts_with("alpha-3 [faux] · default"), "{ids:?}");
    harness.type_text("def");
    assert_eq!(selected_model_id(&harness.render(120).join("\n")).as_deref(), Some("alpha-3"));
    harness.send(keys::ENTER);
    assert_eq!(selected.get(), ["alpha-3"]);
}

#[test]
fn wraps_navigation_and_shows_no_matches_and_cancels() {
    let models = alpha_models();
    let cancelled = Calls::new();
    let harness = Harness::new_shared(120, 30, |tui| {
        ModelSelector::new(
            tui,
            None,
            source(models.clone()),
            Vec::new(),
            |_| {},
            cancelled.counter(),
            Default::default(),
        )
    });
    harness.send(keys::UP);
    assert_eq!(selected_model_id(&harness.render(120).join("\n")).as_deref(), Some("beta-1"));
    harness.send(keys::DOWN);
    assert_eq!(selected_model_id(&harness.render(120).join("\n")).as_deref(), Some("alpha-1"));
    harness.type_text("zzzz");
    assert!(harness.screen().contains("  No matching models"));
    harness.send(keys::ESCAPE);
    assert_eq!(cancelled.len(), 1);
}

/// Ids of `models` that match `query` with the /model selector text, best first.
fn search_ids(models: &[Model], query: &str) -> Vec<String> {
    let texts: Vec<(String, String)> = models
        .iter()
        .map(|model| {
            (model.id.clone(), crate::interactive_components::model_selector::model_selector_search_text(model))
        })
        .collect();
    pi_tui::fuzzy::fuzzy_filter(texts, query, |(_, text)| text.as_str()).into_iter().map(|(id, _)| id).collect()
}

// Upstream #11220.
#[test]
fn search_ranks_models_containing_the_query_above_scattered_matches() {
    let models = [
        model("openrouter", "openai/gpt-oss-120b", "OpenAI: gpt-oss-120b"),
        model("openrouter", "anthropic/claude-sonnet-4.5", "Anthropic: Claude Sonnet 4.5"),
        model("openrouter", "anthropic/claude-opus-4.1", "Anthropic: Claude Opus 4.1"),
        model("anthropic", "claude-sonnet-4-5", "Claude Sonnet 4.5"),
        model("anthropic", "claude-opus-4-1", "Claude Opus 4.1"),
    ];
    let mut top = search_ids(&models, "opus")[..2].to_vec();
    top.sort();
    assert_eq!(top, ["anthropic/claude-opus-4.1", "claude-opus-4-1"]);
}

// Upstream #10065.
#[test]
fn search_ranks_the_typed_model_above_models_sharing_a_long_provider_prefix() {
    let models = [
        model("fireworks", "accounts/fireworks/models/deepseek-v4-flash-0731", ""),
        model("fireworks", "accounts/fireworks/models/kimi-k3", ""),
        model("fireworks", "accounts/fireworks/routers/firerouter", "FireRouter"),
    ];
    assert_eq!(search_ids(&models, "firerouter")[0], "accounts/fireworks/routers/firerouter");
}

// Upstream #5916.
#[test]
fn distinct_model_names_hide_names_that_restate_the_id() {
    use crate::interactive_components::model_selector::distinct_model_name;
    assert_eq!(distinct_model_name("claude-sonnet-4-5", "Claude Sonnet 4.5"), None);
    assert_eq!(distinct_model_name("openai/gpt-oss-120b", "OpenAI: gpt-oss-120b"), None);
    assert_eq!(distinct_model_name("eu.anthropic.claude-sonnet-4-5-20250929-v1:0", "Claude Sonnet 4.5 (EU)"), None);
    assert_eq!(distinct_model_name("gpt-5", ""), None);
    let name = "MiniMax M3 [rasmus@openrouter, parasail]";
    assert_eq!(distinct_model_name("minimax/minimax-m3", name).as_deref(), Some(name));
    assert_eq!(
        distinct_model_name("gemini-3.1-flash-lite-image", "Nano Banana 2 Lite").as_deref(),
        Some("Nano Banana 2 Lite")
    );
    // Single-character words must match a whole id token.
    assert_eq!(distinct_model_name("mistral-small-2603", "Mistral Small 4").as_deref(), Some("Mistral Small 4"));
}

#[test]
fn shows_model_names_that_differ_from_the_id() {
    let _kb = setup();
    let models = vec![
        model("faux", "minimax/minimax-m3", "MiniMax M3 [parasail]"),
        model("faux", "claude-sonnet-4-5", "Claude Sonnet 4.5"),
    ];
    let harness = Harness::new_shared(120, 30, |tui| {
        ModelSelector::new(tui, None, source(models.clone()), Vec::new(), |_| {}, || {}, Default::default())
    });
    let rows: Vec<String> =
        harness.render(120).iter().map(|line| line.chars().skip(4).collect::<String>().trim_end().into()).collect();
    assert!(rows.contains(&"minimax/minimax-m3 (MiniMax M3 [parasail]) [faux]".to_string()), "{rows:?}");
    assert!(rows.contains(&"claude-sonnet-4-5 [faux]".to_string()), "{rows:?}");
}
