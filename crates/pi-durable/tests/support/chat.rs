//! Faux responses and tool registration shared by the harness tests.

use pi_ai::StopReason;
use pi_durable::harness::*;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, faux_assistant_message, faux_text, faux_tool_call_with_id,
};
use serde_json::{Value, json};

use super::harness_support::ChatSetup;

pub fn answer(text: &str) -> FauxResponseStep {
    faux_assistant_message(vec![faux_text(text)], FauxMessageOptions::default()).into()
}

/// A tool-calling answer with one call per `(name, args, id)`.
pub fn calls(list: &[(&str, Value, &str)]) -> FauxResponseStep {
    let blocks =
        list.iter().map(|(name, args, id)| faux_tool_call_with_id(*name, args.clone(), *id)).collect::<Vec<_>>();
    faux_assistant_message(blocks, FauxMessageOptions::stop_reason(StopReason::ToolUse)).into()
}

/// One call of `name` with no arguments.
pub fn call(name: &str, id: &str) -> FauxResponseStep {
    calls(&[(name, json!({}), id)])
}

pub fn add_tool(setup: &ChatSetup, tool: ToolRegistration) {
    let name = format!("tool:{}", tool.name());
    setup.registry.install(Extension::new(name).tool(tool)).unwrap();
}

pub fn no_parameters() -> Value {
    json!({ "type": "object", "properties": {} })
}

/// A tool without parameters whose output is the text `<name> ran`.
pub fn text_tool(name: &str) -> ToolRegistration {
    let text = format!("{name} ran");
    define_tool(name, name, no_parameters(), move |_, _, _| {
        let text = text.clone();
        async move { Ok(ToolExecutionResult::text(text)) }
    })
}
