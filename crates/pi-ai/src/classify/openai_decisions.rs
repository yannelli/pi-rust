//! OpenAI's Decisions API: `POST {baseUrl}/decisions` with `{ model, input, questions }`
//! (upstream `openai-decisions.ts`). The state is sent as JSON text; with images, the input
//! is one user message with the state as `input_text` followed by `input_image` data URLs.

use std::collections::HashMap;

use futures::future::BoxFuture;
use serde_json::{Value, json};

use super::shared::{
    endpoint, error_message, failure, parse_classifier_usage, post_classifier_request, required_number,
};
use super::{
    BoolCriteria, ClassifierAnswer, ClassifierApi, ClassifierContext, ClassifierOptions, ClassifierQuestion,
    ClassifierResult, OrderedMap,
};
use crate::retry::ProviderRequestError;
use crate::types::{JsonObject, Model};

const LABEL: &str = "OpenAI Decisions";

/// The endpoint accepts at most this many image parts per request.
const MAX_IMAGES: usize = 128;

/// Cloudflare in front of api.openai.com answers 504 when a request runs longer than about
/// five seconds. Large inputs hit this limit again on retry, so 504 is not retried.
const NO_RETRY_STATUSES: &[u16] = &[504];

/// Classification through OpenAI's Decisions API. Only OpenAI API keys work on this route.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenAIDecisions;

impl ClassifierApi for OpenAIDecisions {
    fn classify(
        &self,
        model: &Model,
        context: &ClassifierContext,
        options: ClassifierOptions,
    ) -> BoxFuture<'static, ClassifierResult> {
        let (model, context) = (model.clone(), context.clone());
        Box::pin(async move {
            let mut output = ClassifierResult::new(&model);
            if let Err(error) = run(&model, &context, &options, &mut output).await {
                output.fail(decisions_error_message(&error), options.request.cancel.is_cancelled());
            }
            output
        })
    }
}

async fn run(
    model: &Model,
    context: &ClassifierContext,
    options: &ClassifierOptions,
    output: &mut ClassifierResult,
) -> Result<(), ProviderRequestError> {
    if model.api != "openai-decisions" {
        return Err(failure(format!("Unsupported classifier API: {}", model.api)));
    }
    let questions: Vec<Value> = context.questions.iter().map(|(id, question)| wire_question(id, question)).collect();
    let body = json!({ "model": model.id, "input": wire_input(context)?, "questions": questions });
    let url = endpoint(&model.base_url, "decisions");
    let body = post_classifier_request(LABEL, url, model, body, &options.request, NO_RETRY_STATUSES).await?;
    let Some(body) = body.as_object() else {
        return Err(failure(format!("{LABEL} returned an unexpected response")));
    };
    // Set before parsing answers: a request with malformed or refused answers was still billed.
    output.usage = parse_classifier_usage(body.get("usage"), model);
    output.answers = parse_answers(body.get("answers"), context)?;
    Ok(())
}

fn decisions_error_message(error: &ProviderRequestError) -> String {
    if error.status == Some(504) {
        return format!(
            "{LABEL} error (504): the request timed out at the gateway. Very large inputs (above roughly 600K tokens) currently exceed its time limit."
        );
    }
    error_message(error, Some(&format!("{LABEL} error")))
}

/// Predicates have no criteria field, so the meanings of true and false follow the instructions.
fn predicate_instructions(instructions: &str, criteria: &BoolCriteria) -> String {
    let meanings: Vec<String> = [("True", &criteria.when_true), ("False", &criteria.when_false)]
        .into_iter()
        .filter(|(_, meaning)| !meaning.is_empty())
        .map(|(name, meaning)| format!("{name} means: {meaning}"))
        .collect();
    if meanings.is_empty() {
        return instructions.to_string();
    }
    format!("{instructions}\n\n{}", meanings.join("\n"))
}

fn wire_question(name: &str, question: &ClassifierQuestion) -> Value {
    match question {
        ClassifierQuestion::Choice { instructions, criteria } => {
            let choices: Vec<Value> = criteria
                .iter()
                .map(|(value, description)| match description.is_empty() {
                    true => json!({ "value": value }),
                    false => json!({ "value": value, "description": description }),
                })
                .collect();
            json!({ "type": "choice", "name": name, "instructions": instructions, "choices": choices })
        }
        ClassifierQuestion::Score { instructions, criteria } => {
            let levels: Vec<Value> = criteria.iter().map(|label| json!({ "label": label })).collect();
            json!({ "type": "score", "name": name, "instructions": instructions, "levels": levels })
        }
        ClassifierQuestion::Bool { instructions, criteria } => {
            json!({ "type": "predicate", "name": name, "instructions": predicate_instructions(instructions, criteria) })
        }
    }
}

fn wire_input(context: &ClassifierContext) -> Result<Value, ProviderRequestError> {
    let state = Value::Object(context.state.clone()).to_string();
    let images = context.images.as_deref().unwrap_or_default();
    if images.is_empty() {
        return Ok(Value::String(state));
    }
    if images.len() > MAX_IMAGES {
        return Err(failure(format!("{LABEL} accepts at most {MAX_IMAGES} images, got {}", images.len())));
    }
    let mut content = vec![json!({ "type": "input_text", "text": state })];
    content.extend(images.iter().map(|image| {
        json!({ "type": "input_image", "image_url": format!("data:{};base64,{}", image.mime_type, image.data) })
    }));
    Ok(json!([{ "role": "user", "content": content }]))
}

fn choice_probabilities(value: Option<&Value>, id: &str) -> Result<OrderedMap<f64>, ProviderRequestError> {
    let invalid = || failure(format!("{LABEL} returned invalid probabilities for {id}"));
    let entries = value.and_then(Value::as_array).ok_or_else(invalid)?;
    let mut probabilities = OrderedMap::new();
    for entry in entries {
        let name = entry.as_object().and_then(|entry| entry.get("value")?.as_str()).ok_or_else(invalid)?;
        let field = format!("probability for {id}.{name}");
        probabilities.insert(name, required_number(LABEL, entry.get("probability"), &field)?);
    }
    Ok(probabilities)
}

fn parse_answer(
    id: &str,
    question: &ClassifierQuestion,
    answer: &JsonObject,
) -> Result<ClassifierAnswer, ProviderRequestError> {
    let kind = answer.get("type").and_then(Value::as_str);
    if kind == Some("refusal") {
        return Err(failure(format!("{LABEL} refused to answer {id}")));
    }
    let confidence = || required_number(LABEL, answer.get("confidence"), &format!("confidence for {id}"));
    match question {
        ClassifierQuestion::Choice { .. } => {
            let choice = answer.get("choice").and_then(Value::as_str).filter(|_| kind == Some("choice"));
            let Some(choice) = choice else {
                return Err(failure(format!("{LABEL} did not return a choice answer for {id}")));
            };
            Ok(ClassifierAnswer::Choice {
                choice: choice.to_string(),
                probabilities: choice_probabilities(answer.get("probabilities"), id)?,
                confidence: confidence()?,
            })
        }
        ClassifierQuestion::Score { .. } => {
            if kind != Some("score") {
                return Err(failure(format!("{LABEL} did not return a score answer for {id}")));
            }
            let score = required_number(LABEL, answer.get("score"), &format!("score for {id}"))?;
            Ok(ClassifierAnswer::Score { score, confidence: confidence()? })
        }
        ClassifierQuestion::Bool { .. } => {
            if kind != Some("predicate") {
                return Err(failure(format!("{LABEL} did not return a predicate answer for {id}")));
            }
            let probability = required_number(LABEL, answer.get("probability"), &format!("probability for {id}"))?;
            Ok(ClassifierAnswer::Bool { probability })
        }
    }
}

/// Answers are matched to questions by name, so their order in the response does not matter.
fn parse_answers(
    value: Option<&Value>,
    context: &ClassifierContext,
) -> Result<OrderedMap<ClassifierAnswer>, ProviderRequestError> {
    let Some(answers) = value.and_then(Value::as_array) else {
        return Err(failure(format!("{LABEL} returned an unexpected response")));
    };
    let by_name: HashMap<&str, &JsonObject> = answers
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|answer| Some((answer.get("name")?.as_str()?, answer)))
        .collect();
    let mut parsed = OrderedMap::new();
    for (id, question) in context.questions.iter() {
        let Some(answer) = by_name.get(id) else {
            return Err(failure(format!("{LABEL} did not return an answer for {id}")));
        };
        parsed.insert(id, parse_answer(id, question, answer)?);
    }
    Ok(parsed)
}
