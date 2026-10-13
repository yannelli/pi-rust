//! Autocomplete providers extensions stack with `ctx.ui.addAutocompleteProvider` (upstream
//! `autocompleteProviderWrappers`). The chain runs in JavaScript over a proxy of the built-in
//! provider; cursor columns cross as UTF-16 indices.

use pi_js::{HostValue, JsRef};
use pi_tui::AutocompleteItem;
use pi_tui::autocomplete::{
    AutocompleteProvider, AutocompleteSuggestions, CompletionResult, SuggestionOptions, SuggestionsFuture,
    apply_default_completion,
};
use serde_json::{Value, json};

use super::component::ComponentFactory;
use super::ops_ui::{byte_to_utf16, utf16_to_byte};

/// The provider of a chain of extension factories.
pub struct JsAutocompleteProvider {
    host: JsRef,
    chain: JsRef,
    triggers: Vec<String>,
}

impl JsAutocompleteProvider {
    /// Apply `factories` (all of one extension host) in order; `Err` when the host is gone or a
    /// factory throws.
    pub fn build(factories: &[ComponentFactory]) -> Result<Self, String> {
        let first = factories.first().ok_or("No autocomplete providers")?;
        let host = first.host()?;
        let ids: Vec<u64> = factories.iter().map(ComponentFactory::id).collect();
        let args = vec![HostValue::Json(json!(first.target())), HostValue::Json(json!(ids))];
        let chain = host
            .blocking_call_method_opt_ref("autocompleteChain", args)
            .map_err(|error| error.message)?
            .ok_or("The autocomplete chain is empty")?;
        let reference = chain.clone();
        let triggers = chain
            .handle()
            .blocking_with(move |scope| {
                let value = scope.value(&reference)?;
                let triggers = value
                    .as_object()
                    .and_then(|object| object.get::<_, Vec<String>>("triggerCharacters").ok())
                    .unwrap_or_default();
                Ok(triggers)
            })
            .unwrap_or_default();
        Ok(Self { host, chain, triggers })
    }

    fn args(&self, method: &str, args: Value) -> Vec<HostValue> {
        vec![HostValue::Ref(self.chain.clone()), HostValue::Json(json!(method)), HostValue::Json(args)]
    }

    fn blocking(&self, method: &str, args: Value) -> Option<Value> {
        let result = self.host.blocking_call_method("autocomplete", self.args(method, args)).ok()?;
        Some(result.into_json()).filter(|value| !value.is_null())
    }
}

fn line(lines: &[String], index: usize) -> &str {
    lines.get(index).map_or("", String::as_str)
}

fn item_json(item: &AutocompleteItem) -> Value {
    json!({"value": item.value, "label": item.label, "description": item.description})
}

/// `{ items, prefix }` from JavaScript.
pub(crate) fn parse_suggestions(value: &Value) -> Option<AutocompleteSuggestions> {
    let items = value["items"]
        .as_array()?
        .iter()
        .filter_map(|item| {
            let value = item["value"].as_str()?;
            let mut parsed = AutocompleteItem::new(value, item["label"].as_str().unwrap_or(value));
            parsed.description = item["description"].as_str().map(str::to_string);
            Some(parsed)
        })
        .collect();
    Some(AutocompleteSuggestions { items, prefix: value["prefix"].as_str().unwrap_or_default().to_string() })
}

impl AutocompleteProvider for JsAutocompleteProvider {
    fn trigger_characters(&self) -> Vec<String> {
        self.triggers.clone()
    }

    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: SuggestionOptions,
    ) -> SuggestionsFuture {
        let col = byte_to_utf16(line(lines, cursor_line), cursor_col);
        let args = self.args("getSuggestions", json!([lines, cursor_line, col, {"force": options.force}]));
        let host = self.host.clone();
        Box::pin(async move {
            let result = host.call_method("autocomplete", args).await.ok()?.into_json();
            if options.signal.is_aborted() {
                return None;
            }
            parse_suggestions(&result)
        })
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        let col = byte_to_utf16(line(lines, cursor_line), cursor_col);
        let applied = self.blocking("applyCompletion", json!([lines, cursor_line, col, item_json(item), prefix]));
        let parsed = applied.and_then(|value| {
            let lines: Vec<String> = serde_json::from_value(value["lines"].clone()).ok()?;
            let cursor_line = value["cursorLine"].as_u64()? as usize;
            let cursor_col = utf16_to_byte(line(&lines, cursor_line), value["cursorCol"].as_u64()? as usize);
            Some(CompletionResult { lines, cursor_line, cursor_col })
        });
        parsed.unwrap_or_else(|| apply_default_completion(lines, cursor_line, cursor_col, item, prefix))
    }

    fn should_trigger_file_completion(&self, lines: &[String], cursor_line: usize, cursor_col: usize) -> bool {
        let col = byte_to_utf16(line(lines, cursor_line), cursor_col);
        let result = self.blocking("shouldTriggerFileCompletion", json!([lines, cursor_line, col]));
        result.and_then(|value| value.as_bool()).unwrap_or(true)
    }
}
