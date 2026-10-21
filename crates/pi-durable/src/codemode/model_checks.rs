//! Checks of the contexts scripts pass to `models.classify()` and `models.generateImages()`, so mistakes fail with
//! the expected shape instead of a provider error.

use pi_codemode::js::js_stringify;
use serde_json::Value;

/// A script value in an error message: `undefined`, `a string`, `an array`, or its keys (`{ prompt }`).
pub(super) fn describe_value(value: Option<&Value>) -> String {
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

const IMAGES_CONTEXT_SHAPE: &str = "{ input: [{ type: \"text\", text: <prompt> }, ...optional { type: \"image\", data: <base64>, mimeType } references] }";

fn is_image_block(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("image")
        && value.get("data").is_some_and(Value::is_string)
        && value.get("mimeType").is_some_and(Value::is_string)
}

fn all_strings<'a>(mut values: impl ExactSizeIterator<Item = &'a Value>) -> bool {
    values.len() > 0 && values.all(Value::is_string)
}

/// Check one classifier question; the problem, if any.
fn check_question(at: &str, question: &Value) -> Option<String> {
    let Value::Object(question) = question else {
        return Some(format!("{at} must be a question object, got {}", describe_value(Some(question))));
    };
    if !question.get("instructions").is_some_and(Value::is_string) {
        return Some(format!("{at}.instructions must be a string"));
    }
    let criteria = question.get("criteria");
    match question.get("type").and_then(Value::as_str) {
        Some("choice") if !matches!(criteria, Some(Value::Object(labels)) if all_strings(labels.values())) => {
            Some(format!("{at} is a \"choice\" question, so criteria must map each label to its meaning"))
        }
        Some("score") if !matches!(criteria, Some(Value::Array(levels)) if all_strings(levels.iter())) => {
            Some(format!("{at} is a \"score\" question, so criteria must list the levels as strings, lowest first"))
        }
        Some("bool") => {
            let valid = criteria.is_some_and(|criteria| {
                criteria.is_object()
                    && criteria.get("true").is_some_and(Value::is_string)
                    && criteria.get("false").is_some_and(Value::is_string)
            });
            (!valid)
                .then(|| format!("{at} is a \"bool\" question, so criteria must be {{ true: string, false: string }}"))
        }
        Some("choice" | "score") => None,
        _ => {
            let kind = question.get("type").map_or_else(|| "undefined".into(), js_stringify);
            Some(format!("{at}.type must be \"choice\", \"score\", or \"bool\", got {kind}"))
        }
    }
}

/// Check a script's classifier context and return it unchanged.
pub(super) fn check_classifier_context(context: Option<&Value>) -> Result<Value, String> {
    let fail = |problem: String| format!("models.classify() {problem}. Expected context: {CLASSIFIER_CONTEXT_SHAPE}.");
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
                let got = describe_value(Some(image));
                return Err(fail(format!("context.images[{index}] must be an image block, got {got}")));
            }
        }
        Some(other) => {
            return Err(fail(format!("context.images must be an array, got {}", describe_value(Some(other)))));
        }
    }
    let questions = match fields.get("questions") {
        Some(Value::Object(questions)) if !questions.is_empty() => questions,
        other => {
            let got = describe_value(other);
            return Err(fail(format!("context.questions must map question IDs to questions, got {got}")));
        }
    };
    for (id, question) in questions {
        if let Some(problem) = check_question(&format!("context.questions.{id}"), question) {
            return Err(fail(problem));
        }
    }
    Ok(context.clone())
}

/// Check a script's image context, so mistakes such as `{ prompt }` fail with the expected shape.
pub(super) fn check_images_context(context: Option<&Value>) -> Result<Value, String> {
    let fail =
        |problem: String| format!("models.generateImages() {problem}. Expected context: {IMAGES_CONTEXT_SHAPE}.");
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
            let got = describe_value(Some(block));
            return Err(fail(format!("context.input[{index}] must be a text or image block, got {got}")));
        }
    }
    Ok(context.clone())
}
