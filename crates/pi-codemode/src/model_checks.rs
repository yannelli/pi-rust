//! Checks of the contexts scripts pass to `models.classify()` and `models.generateImages()`, so
//! mistakes fail with the expected shape instead of a provider error.

use serde_json::Value;

use crate::js::js_stringify;

/// A script value in an error message: `undefined`, `a string`, `an array`, or its keys (`{ prompt }`).
pub(crate) fn describe_value(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Array(items)) => if items.is_empty() { "an empty array" } else { "an array" }.into(),
        Some(Value::Object(fields)) if fields.is_empty() => "{}".into(),
        Some(Value::Object(fields)) => {
            let keys: Vec<&str> = fields.keys().take(6).map(String::as_str).collect();
            format!("{{ {}{} }}", keys.join(", "), if fields.len() > 6 { ", ..." } else { "" })
        }
        Some(Value::String(_)) => "a string".into(),
        Some(Value::Number(_)) => "a number".into(),
        Some(Value::Bool(_)) => "a boolean".into(),
    }
}

const CLASSIFIER_CONTEXT_SHAPE: &str = "{ state: { ... }, images?: [{ type: \"image\", data: <base64>, mimeType }], questions: { <id>: { type: \"choice\", instructions, criteria: { <label>: <meaning> } } | { type: \"score\", instructions, criteria: [<lowest level>, ..., <highest level>] } | { type: \"bool\", instructions, criteria: { true: <meaning>, false: <meaning> } } } }";

fn is_image_block(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("image")
        && value.get("data").is_some_and(Value::is_string)
        && value.get("mimeType").is_some_and(Value::is_string)
}

fn all_strings<'a>(mut values: impl ExactSizeIterator<Item = &'a Value>) -> bool {
    values.len() > 0 && values.all(Value::is_string)
}

/// Check a script's classifier context and return it unchanged.
pub(crate) fn check_classifier_context(context: Option<&Value>, docs_path: &str) -> Result<Value, String> {
    let fail = |problem: String| {
        format!(
            "models.classify() {problem}. Expected context: {CLASSIFIER_CONTEXT_SHAPE}. See \"Classify\" in {docs_path}."
        )
    };
    let Some(context @ Value::Object(fields)) = context else {
        return Err(fail(format!("expects a context object as its second argument, got {}", describe_value(context))));
    };
    if !fields.get("state").is_some_and(Value::is_object) {
        return Err(fail(format!("context.state must be an object, got {}", describe_value(fields.get("state")))));
    }
    match fields.get("images") {
        None => {}
        Some(Value::Array(images)) => {
            if let Some((index, image)) = images.iter().enumerate().find(|(_, image)| !is_image_block(image)) {
                return Err(fail(format!(
                    "context.images[{index}] must be an image block, got {}",
                    describe_value(Some(image))
                )));
            }
        }
        Some(other) => {
            return Err(fail(format!("context.images must be an array, got {}", describe_value(Some(other)))));
        }
    }
    let questions = match fields.get("questions") {
        Some(Value::Object(questions)) if !questions.is_empty() => questions,
        other => {
            return Err(fail(format!(
                "context.questions must map question IDs to questions, got {}",
                describe_value(other)
            )));
        }
    };
    for (id, question) in questions {
        let at = format!("context.questions.{id}");
        let Value::Object(question) = question else {
            return Err(fail(format!("{at} must be a question object, got {}", describe_value(Some(question)))));
        };
        if !question.get("instructions").is_some_and(Value::is_string) {
            return Err(fail(format!("{at}.instructions must be a string")));
        }
        let criteria = question.get("criteria");
        match question.get("type").and_then(Value::as_str) {
            Some("choice") => {
                if !matches!(criteria, Some(Value::Object(labels)) if all_strings(labels.values())) {
                    return Err(fail(format!(
                        "{at} is a \"choice\" question, so criteria must map each label to its meaning"
                    )));
                }
            }
            Some("score") => {
                if !matches!(criteria, Some(Value::Array(levels)) if all_strings(levels.iter())) {
                    return Err(fail(format!(
                        "{at} is a \"score\" question, so criteria must list the levels as strings, lowest first"
                    )));
                }
            }
            Some("bool") => {
                let valid = criteria.is_some_and(|criteria| {
                    criteria.is_object()
                        && criteria.get("true").is_some_and(Value::is_string)
                        && criteria.get("false").is_some_and(Value::is_string)
                });
                if !valid {
                    return Err(fail(format!(
                        "{at} is a \"bool\" question, so criteria must be {{ true: string, false: string }}"
                    )));
                }
            }
            _ => {
                let kind = question.get("type").map_or_else(|| "undefined".into(), js_stringify);
                return Err(fail(format!("{at}.type must be \"choice\", \"score\", or \"bool\", got {kind}")));
            }
        }
    }
    Ok(context.clone())
}

/// Check a script's image context, so mistakes such as `{ prompt }` fail with the expected shape.
pub(crate) fn check_images_context(context: Option<&Value>, docs_path: &str) -> Result<Value, String> {
    let fail = |problem: String| {
        format!(
            "models.generateImages() {problem}. Expected context: {{ input: [{{ type: \"text\", text: <prompt> }}, ...optional {{ type: \"image\", data: <base64>, mimeType }} references] }}. See \"Generate images\" in {docs_path}."
        )
    };
    let Some(context @ Value::Object(fields)) = context else {
        return Err(fail(format!("expects a context object as its second argument, got {}", describe_value(context))));
    };
    let input = match fields.get("input") {
        Some(Value::Array(input)) if !input.is_empty() => input,
        other => {
            return Err(fail(format!(
                "context.input must be a non-empty array of blocks, got {}",
                describe_value(other)
            )));
        }
    };
    for (index, block) in input.iter().enumerate() {
        let is_text = block.get("type").and_then(Value::as_str) == Some("text")
            && block.get("text").is_some_and(Value::is_string);
        if !is_text && !is_image_block(block) {
            return Err(fail(format!(
                "context.input[{index}] must be a text or image block, got {}",
                describe_value(Some(block))
            )));
        }
    }
    Ok(context.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn describes_script_values() {
        assert_eq!(describe_value(None), "undefined");
        assert_eq!(describe_value(Some(&json!([]))), "an empty array");
        assert_eq!(describe_value(Some(&json!({ "data": 1 }))), "{ data }");
        assert_eq!(
            describe_value(Some(&json!({ "a": 1, "b": 2, "c": 3, "d": 4, "e": 5, "f": 6, "g": 7 }))),
            "{ a, b, c, d, e, f, ... }"
        );
        assert_eq!(describe_value(Some(&json!(true))), "a boolean");
    }
}
