//! The per-model thinking level submenu of the settings selector.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use pi_ai::simple_options::supported_thinking_levels;
use pi_ai::{Model, ThinkingLevel};
use pi_tui::components::SelectItem;
use pi_tui::components::SelectListLayoutOptions;
use pi_tui::components::settings_list::{SubmenuDone, SubmenuFactory};
use pi_tui::{ComponentRef, component};

use super::super::common::fg;
use super::SharedCallbacks;
use super::submenu::{StepContext, StepText, SteppedSubmenu, SteppedSubmenuOptions, SteppedSubmenuStep};
use crate::theme::ThemeColor;

const CLEAR_OVERRIDE_VALUE: &str = "__clear__";

pub(super) fn thinking_description(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "No reasoning",
        ThinkingLevel::Minimal => "Very brief reasoning (~1k tokens)",
        ThinkingLevel::Low => "Light reasoning (~2k tokens)",
        ThinkingLevel::Medium => "Moderate reasoning (~8k tokens)",
        ThinkingLevel::High => "Deep reasoning (~16k tokens)",
        ThinkingLevel::Xhigh => "Extra-high reasoning (~32k tokens)",
        ThinkingLevel::Max => "Maximum reasoning",
    }
}

/// The settings key of a model: `provider/modelId`.
pub(super) fn model_setting_key(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

fn model_display_label(model: &Model) -> String {
    format!("{} [{}]", model.id, model.provider)
}

fn model_item_label(model: &Model) -> String {
    format!("{} {}", model.id, fg(ThemeColor::Muted, &format!("[{}]", model.provider)))
}

/// `none` or `<n> configured`.
pub(super) fn overrides_summary(overrides: &BTreeMap<String, ThinkingLevel>) -> String {
    if overrides.is_empty() { "none".into() } else { format!("{} configured", overrides.len()) }
}

fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b))
}

/// Models and overrides that the per-model thinking submenu reads and updates.
pub(super) struct ModelThinkingData {
    pub models: Vec<Model>,
    pub current_model_key: Option<String>,
    pub current_default_model_key: Option<String>,
    pub global_level: ThinkingLevel,
    pub overrides: Rc<RefCell<BTreeMap<String, ThinkingLevel>>>,
}

impl ModelThinkingData {
    fn model(&self, key: &str) -> Option<&Model> {
        self.models.iter().find(|model| model_setting_key(model) == key)
    }

    fn model_items(&self) -> Vec<SelectItem> {
        let rank = |key: &str| {
            if Some(key) == self.current_model_key.as_deref() {
                0
            } else if Some(key) == self.current_default_model_key.as_deref() {
                1
            } else {
                2
            }
        };
        let mut sorted: Vec<&Model> = self.models.iter().collect();
        sorted.sort_by(|a, b| {
            let (a_rank, b_rank) = (rank(&model_setting_key(a)), rank(&model_setting_key(b)));
            if a_rank < 2 || b_rank < 2 {
                return a_rank.cmp(&b_rank);
            }
            locale_compare(&a.provider, &b.provider)
        });
        let overrides = self.overrides.borrow();
        let mut items: Vec<SelectItem> = sorted
            .into_iter()
            .map(|model| {
                let key = model_setting_key(model);
                let item = SelectItem::new(&key, &model_item_label(model));
                match overrides.get(&key) {
                    Some(level) => item.with_description(level.as_str()),
                    None => item,
                }
            })
            .collect();
        if items.is_empty() {
            items.push(
                SelectItem::new("__none__", "No models available")
                    .with_description("Log in to a provider or configure an API key first"),
            );
        }
        items
    }

    fn level_items(&self, model_key: &str) -> Vec<SelectItem> {
        let Some(model) = self.model(model_key) else { return Vec::new() };
        let active = self.overrides.borrow().get(model_key).copied();
        let mut items: Vec<SelectItem> = supported_thinking_levels(model)
            .into_iter()
            .map(|level| {
                let marker = if Some(level) == active { "✓ " } else { "  " };
                SelectItem::new(level.as_str(), &format!("{marker}{}", level.as_str()))
                    .with_description(thinking_description(level))
            })
            .collect();
        if active.is_some() {
            let description = format!("Revert to global default ({})", self.global_level.as_str());
            items.push(SelectItem::new(CLEAR_OVERRIDE_VALUE, "  (clear override)").with_description(&description));
        }
        items
    }
}

/// The per-model thinking level submenu: pick a model, then its level, and repeat.
pub(super) fn model_thinking_factory(data: Rc<ModelThinkingData>, callbacks: SharedCallbacks) -> SubmenuFactory {
    Rc::new(move |_current: &str, done: SubmenuDone| -> ComponentRef {
        let context_key = |context: &StepContext| context.get("model").cloned().unwrap_or_default();
        let (options_data, preselect_data, title_data, levels_data, level_preselect) =
            (data.clone(), data.clone(), data.clone(), data.clone(), data.clone());
        let steps = vec![
            SteppedSubmenuStep {
                key: "model".into(),
                title: "Per-Model Thinking Level".into(),
                description: "Select a model to configure".into(),
                options: Box::new(move |_| options_data.model_items()),
                preselect: Some(Box::new(move |_| {
                    preselect_data
                        .current_model_key
                        .clone()
                        .or_else(|| preselect_data.current_default_model_key.clone())
                })),
                searchable: true,
                layout: Some(SelectListLayoutOptions {
                    min_primary_column_width: Some(12),
                    max_primary_column_width: Some(46),
                    truncate_primary: None,
                }),
            },
            SteppedSubmenuStep {
                key: "level".into(),
                title: StepText::Dynamic(Box::new(move |context| {
                    let key = context_key(context);
                    let label = title_data.model(&key).map_or(key.clone(), model_display_label);
                    format!("Thinking Level for {label}")
                })),
                description: "Select default thinking level for this model".into(),
                options: Box::new(move |context| levels_data.level_items(&context_key(context))),
                preselect: Some(Box::new(move |context| {
                    let overrides = level_preselect.overrides.borrow();
                    overrides.get(&context_key(context)).map(|level| level.as_str().to_string())
                })),
                searchable: false,
                layout: None,
            },
        ];
        let (complete_data, cancel_data, callbacks) = (data.clone(), data.clone(), callbacks.clone());
        component(SteppedSubmenu::new(
            steps,
            move |selections: &StepContext| {
                let key = selections.get("model").cloned().unwrap_or_default();
                let Some(model) = complete_data.model(&key) else { return };
                let level = selections.get("level").map(String::as_str).unwrap_or_default();
                if level == CLEAR_OVERRIDE_VALUE {
                    (callbacks.borrow_mut().on_model_thinking_level_remove)(&model.provider, &model.id);
                    complete_data.overrides.borrow_mut().remove(&key);
                } else if let Ok(level) = level.parse::<ThinkingLevel>() {
                    (callbacks.borrow_mut().on_model_thinking_level_change)(&model.provider, &model.id, level);
                    complete_data.overrides.borrow_mut().insert(key, level);
                }
            },
            move || done.done(Some(&overrides_summary(&cancel_data.overrides.borrow()))),
            SteppedSubmenuOptions { loop_back: true, ..SteppedSubmenuOptions::default() },
        ))
    })
}
