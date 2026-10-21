//! Apply an account's model list to a catalog (pi-rust extension): drop chat models the list
//! does not name, take the listed limits, and add listed chat models the catalog lacks as copies
//! of the closest catalog model.

use std::cmp::Reverse;
use std::collections::HashSet;

use serde_json::Value;

use crate::models::is_chat_model;
use crate::types::Model;

/// One chat model of an account's list.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Listed {
    pub id: String,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
}

impl Listed {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), ..Self::default() }
    }
}

/// Which listed models the catalog lacks become new models.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AddRule {
    /// The list has only chat models of this provider.
    Any,
    /// Only ids of a model family the catalog has, and no embedding, audio, image, video,
    /// moderation, realtime, instruct, or search models.
    CatalogFamilies,
}

/// Id parts that mark models pi cannot chat with.
const NON_CHAT_PARTS: [&str; 20] = [
    "embed",
    "embedding",
    "tts",
    "audio",
    "transcribe",
    "whisper",
    "image",
    "imagen",
    "imagine",
    "dall",
    "video",
    "veo",
    "sora",
    "moderation",
    "realtime",
    "aqa",
    "babbage",
    "davinci",
    "instruct",
    "search",
];

/// A positive number at `pointer`, as an integer.
pub(crate) fn positive(entry: &Value, pointer: &str) -> Option<u64> {
    entry.pointer(pointer).and_then(Value::as_f64).filter(|value| *value > 0.0).map(|value| value as u64)
}

/// `id` without a trailing `-YYYYMMDD` or `-YYYY-MM-DD` snapshot date.
fn undated(id: &str) -> &str {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if let Some((base, date)) = id.rsplit_once('-')
        && date.len() == 8
        && digits(date)
    {
        return base;
    }
    let parts: Vec<&str> = id.rsplitn(4, '-').collect();
    if let [day, month, year, base] = parts.as_slice()
        && year.len() == 4
        && month.len() == 2
        && day.len() == 2
        && [day, month, year].iter().all(|part| digits(part))
    {
        return base;
    }
    id
}

/// The same model: equal ids, or an alias and its dated snapshot.
fn same_model(catalog_id: &str, listed_id: &str) -> bool {
    catalog_id == listed_id || undated(listed_id) == catalog_id || undated(catalog_id) == listed_id
}

fn family(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

fn addable(id: &str, rule: AddRule, families: &HashSet<&str>) -> bool {
    match rule {
        AddRule::Any => true,
        AddRule::CatalogFamilies => {
            let mut parts = id.split(['-', '.', '_', ':', '/']);
            families.contains(family(id)) && !parts.any(|part| NON_CHAT_PARTS.contains(&part))
        }
    }
}

/// The catalog chat model sharing the longest id prefix with `id`, then the closest length.
fn closest<'a>(chats: &[&'a Model], id: &str) -> Option<&'a Model> {
    let shared = |other: &str| other.chars().zip(id.chars()).take_while(|(a, b)| a == b).count();
    chats
        .iter()
        .copied()
        .max_by_key(|&model| (shared(&model.id), Reverse(model.id.len().abs_diff(id.len())), model.id.as_str()))
}

fn with_limits(mut model: Model, listed: &Listed) -> Model {
    model.context_window = listed.context_window.unwrap_or(model.context_window);
    model.max_tokens = listed.max_tokens.unwrap_or(model.max_tokens);
    model
}

/// Keep the catalog's chat models that `listed` names, with its limits, then add the listed
/// models the catalog lacks. Image and classifier models stay. An empty list changes nothing.
pub(crate) fn reconcile(catalog: Vec<Model>, listed: &[Listed], rule: AddRule) -> Vec<Model> {
    if listed.is_empty() {
        return catalog;
    }
    let chats: Vec<Model> = catalog.iter().filter(|model| is_chat_model(model)).cloned().collect();
    let chat_refs: Vec<&Model> = chats.iter().collect();
    let families: HashSet<&str> = chats.iter().map(|model| family(&model.id)).collect();
    let find = |id: &str| {
        let exact = listed.iter().find(|entry| entry.id == id);
        exact.or_else(|| listed.iter().find(|entry| same_model(id, &entry.id)))
    };
    let mut models: Vec<Model> = catalog
        .into_iter()
        .filter_map(|model| match is_chat_model(&model) {
            false => Some(model),
            true => find(&model.id).map(|entry| with_limits(model, entry)),
        })
        .collect();
    for entry in listed {
        let known = chats.iter().any(|model| same_model(&model.id, &entry.id));
        let added = models.iter().any(|model| same_model(&model.id, &entry.id));
        if known || added || !addable(&entry.id, rule, &families) {
            continue;
        }
        let Some(base) = closest(&chat_refs, &entry.id) else { continue };
        let name = entry.name.clone().unwrap_or_else(|| entry.id.clone());
        let model = Model { id: entry.id.clone(), name, ..base.clone() };
        models.push(with_limits(model, entry));
    }
    models
}
