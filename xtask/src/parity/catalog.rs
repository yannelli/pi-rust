//! Section 6: provider ids and model ids of the upstream catalog against `crates/pi-ai/models.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use super::parse::ts_members;
use super::report::{Item, Section};

const ID: &str = "models";
const TITLE: &str = "Model catalog";
const GENERATED: &str = "packages/ai/src/models.generated.ts";
const DATA_DIR: &str = "packages/ai/src/providers/data";

/// Models of one provider as `(type, id)` pairs.
type Models = BTreeSet<(String, String)>;
type Catalog = BTreeMap<String, Models>;

/// Provider ids from the keys of `MODELS` in `models.generated.ts`.
pub fn generated_providers(src: &str) -> Option<Vec<String>> {
    ts_members(src, &["export const MODELS"]).map(|(_, names)| names)
}

/// `(type, id)` pairs from one upstream provider file: `{ "<api>": { "<type>:<id>": model } }`.
pub fn provider_models(text: &str) -> Result<Models, String> {
    let Value::Object(groups) = serde_json::from_str(text).map_err(|error| error.to_string())? else {
        return Err("expected an object of API groups".into());
    };
    let keys = groups.values().filter_map(Value::as_object).flat_map(|group| group.keys());
    Ok(keys.filter_map(|key| key.split_once(':')).map(|(kind, id)| (kind.to_string(), id.to_string())).collect())
}

/// Models per provider from the flat `models.json` array.
pub fn bundled_models(text: &str) -> Result<Catalog, String> {
    let models: Vec<Value> = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let mut catalog = Catalog::new();
    for model in &models {
        let field = |name: &str| model.get(name).and_then(Value::as_str);
        if let (Some(provider), Some(id)) = (field("provider"), field("id")) {
            catalog
                .entry(provider.to_string())
                .or_default()
                .insert((field("type").unwrap_or("chat").into(), id.into()));
        }
    }
    Ok(catalog)
}

/// A short description of a set of models: counts per type and the first few ids.
fn describe(models: &Models) -> String {
    let mut by_type: BTreeMap<&str, usize> = BTreeMap::new();
    for (kind, _) in models {
        *by_type.entry(kind).or_default() += 1;
    }
    let counts: Vec<String> = by_type.iter().map(|(kind, count)| format!("{kind} {count}")).collect();
    let ids: Vec<&str> = models.iter().take(5).map(|(_, id)| id.as_str()).collect();
    let more = models.len().saturating_sub(5);
    let tail = if more > 0 { format!(", and {more} more") } else { String::new() };
    format!("{} ({}): {}{tail}", models.len(), counts.join(", "), ids.join(", "))
}

pub fn compare(upstream: &Catalog, bundled: &Catalog) -> (Vec<Item>, Vec<Item>) {
    let (mut missing, mut extra) = (Vec::new(), Vec::new());
    for provider in upstream.keys().chain(bundled.keys()).collect::<BTreeSet<_>>() {
        let empty = Models::new();
        let (up, rust) = (upstream.get(provider).unwrap_or(&empty), bundled.get(provider).unwrap_or(&empty));
        if up.is_empty() && upstream.contains_key(provider) && !bundled.contains_key(provider) {
            missing.push(Item::new(provider.as_str()).detail("provider (no model data upstream)"));
        }
        for (list, only, other) in [(&mut missing, up, rust), (&mut extra, rust, up)] {
            let only: Models = only.difference(other).cloned().collect();
            if !only.is_empty() {
                let what = if other.is_empty() { "provider" } else { "models" };
                list.push(Item::new(provider.as_str()).detail(format!("{what} {}", describe(&only))));
            }
        }
    }
    (missing, extra)
}

pub fn section(upstream: &Path, root: &Path) -> Section {
    let skipped = |reason: String| Section::skipped(ID, TITLE, reason);
    let data = upstream.join(DATA_DIR);
    let providers = match std::fs::read_to_string(upstream.join(GENERATED)) {
        Ok(src) => generated_providers(&src),
        Err(_) => data_providers(&data),
    };
    let Some(providers) = providers.filter(|providers| !providers.is_empty()) else {
        return skipped(format!("upstream file not found or without a MODELS table: {GENERATED}"));
    };
    if !data.is_dir() {
        return skipped(format!(
            "{DATA_DIR} is generated and not in git. Run `npm run hydrate-model-data` in packages/ai of the upstream checkout first"
        ));
    }
    let mut catalog = Catalog::new();
    let mut unreadable = Vec::new();
    for provider in &providers {
        let text = std::fs::read_to_string(data.join(format!("{provider}.json"))).map_err(|error| error.to_string());
        let models = text.and_then(|text| provider_models(&text)).unwrap_or_default();
        if models.is_empty() {
            unreadable.push(provider.as_str());
        }
        catalog.insert(provider.clone(), models);
    }
    let bundled_path = root.join("crates/pi-ai/models.json");
    let bundled =
        match std::fs::read_to_string(&bundled_path).map_err(|e| e.to_string()).and_then(|t| bundled_models(&t)) {
            Ok(bundled) => bundled,
            Err(error) => return skipped(format!("cannot read crates/pi-ai/models.json: {error}")),
        };
    let mut section = Section::new(ID, TITLE);
    let total = |catalog: &Catalog| catalog.values().map(BTreeSet::len).sum::<usize>();
    section.notes.push(format!(
        "Upstream: {} providers, {} models. pi-rust: {} providers, {} models.",
        catalog.len(),
        total(&catalog),
        bundled.len(),
        total(&bundled)
    ));
    if !unreadable.is_empty() {
        section.notes.push(format!("No model data found upstream for: {}.", unreadable.join(", ")));
    }
    (section.missing, section.extra) = compare(&catalog, &bundled);
    let table = counts_table(&catalog, &bundled);
    section.collapsed = Some((format!("Model counts per provider ({})", table.len() - 2), table));
    section
}

/// A Markdown table of model counts per provider on both sides.
fn counts_table(upstream: &Catalog, bundled: &Catalog) -> Vec<String> {
    let mut lines = vec!["| Provider | Upstream | pi-rust |".to_string(), "|---|---:|---:|".to_string()];
    for provider in upstream.keys().chain(bundled.keys()).collect::<BTreeSet<_>>() {
        let count = |catalog: &Catalog| catalog.get(provider).map_or(0, BTreeSet::len);
        lines.push(format!("| {provider} | {} | {} |", count(upstream), count(bundled)));
    }
    lines
}

fn data_providers(data: &Path) -> Option<Vec<String>> {
    let entries = std::fs::read_dir(data).ok()?;
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.strip_suffix(".json").map(String::from))
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_from_the_models_table() {
        let src = "import { A } from \"./a.ts\";\nexport const MODELS: {\n\treadonly \"amazon-bedrock\": typeof A;\n\treadonly \"zai\": typeof Z;\n} = {\n\t\"amazon-bedrock\": A,\n};\nexport const IMAGE_MODELS: {\n\treadonly \"other\": typeof B;\n} = {};";
        assert_eq!(generated_providers(src).unwrap(), ["amazon-bedrock", "zai"]);
    }

    #[test]
    fn models_from_both_formats() {
        let up = provider_models(
            r#"{"anthropic-messages": {"chat:claude-a": {}, "image:img:1": {}}, "other": {"chat:claude-b": {}}}"#,
        )
        .unwrap();
        assert_eq!(up.len(), 3);
        assert!(up.contains(&("image".to_string(), "img:1".to_string())));
        let bundled =
            bundled_models(r#"[{"id":"claude-a","provider":"anthropic","type":"chat"},{"id":"x","provider":"solo"}]"#)
                .unwrap();
        assert_eq!(bundled["solo"].iter().next().unwrap(), &("chat".to_string(), "x".to_string()));
        assert!(provider_models("[]").is_err());
    }

    #[test]
    fn compares_provider_and_model_ids() {
        let models = |ids: &[&str]| -> Models { ids.iter().map(|id| ("chat".to_string(), id.to_string())).collect() };
        let upstream = Catalog::from([
            ("a".into(), models(&["1", "2", "3"])),
            ("new".into(), models(&["9"])),
            ("same".into(), models(&["1"])),
        ]);
        let bundled = Catalog::from([
            ("a".into(), models(&["1", "4"])),
            ("old".into(), models(&["7"])),
            ("same".into(), models(&["1"])),
        ]);
        let (missing, extra) = compare(&upstream, &bundled);
        let names = |items: &[Item]| items.iter().map(|item| item.name.clone()).collect::<Vec<_>>();
        assert_eq!((names(&missing), names(&extra)), (vec!["a".into(), "new".into()], vec!["a".into(), "old".into()]));
        assert_eq!(missing[0].detail, "models 2 (chat 2): 2, 3");
        assert_eq!(missing[1].detail, "provider 1 (chat 1): 9");
    }
}
