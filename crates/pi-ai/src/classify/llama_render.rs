//! Prompt rendering and answer math for llama.cpp classification (upstream `llama-cpp-classify.ts`).

use serde::Serialize;
use serde_json::ser::{PrettyFormatter, Serializer};

use super::{ClassifierAnswer, ClassifierContext, ClassifierQuestion, OrderedMap};
use crate::types::JsonObject;

const CHOICE_LABELS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const SCORE_LABELS: &str = "0123456789";
const BOOL_LABELS: [&str; 2] = ["Yes", "No"];

/// One question rendered for the model.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LabeledQuestion {
    /// User message content: the state, the question and its answer labels.
    pub content: String,
    /// Answer labels the model can emit, in the order of `keys`.
    pub labels: Vec<String>,
    /// Answer key each label stands for: choice keys, level indices, or `true`/`false`.
    pub keys: Vec<String>,
}

/// The server root: pi's llama.cpp models use the OpenAI-compatible `/v1` URL as their base URL.
pub(crate) fn llama_server_root(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

/// `JSON.stringify(state, null, 1)`: pretty JSON with a one-space indent.
fn render_state(state: &JsonObject) -> String {
    let mut buffer = Vec::new();
    let mut serializer = Serializer::with_formatter(&mut buffer, PrettyFormatter::with_indent(b" "));
    // Serializing a JSON object into memory cannot fail.
    let _ = state.serialize(&mut serializer);
    format!("State:\n{}", String::from_utf8_lossy(&buffer))
}

fn char_labels(labels: &str, count: usize) -> Vec<String> {
    labels.chars().take(count).map(String::from).collect()
}

/// The answer labels of a question and the keys they stand for. Fails for unsupported option counts.
fn question_labels(question: &ClassifierQuestion) -> Result<(Vec<String>, Vec<String>), String> {
    match question {
        ClassifierQuestion::Choice { criteria, .. } => {
            let (count, max) = (criteria.len(), CHOICE_LABELS.len());
            if !(2..=max).contains(&count) {
                return Err(format!("A choice question needs 2 to {max} options, got {count}"));
            }
            Ok((char_labels(CHOICE_LABELS, count), criteria.keys().map(String::from).collect()))
        }
        ClassifierQuestion::Score { criteria, .. } => {
            let (count, max) = (criteria.len(), SCORE_LABELS.len());
            if !(2..=max).contains(&count) {
                return Err(format!("A score question needs 2 to {max} levels, got {count}"));
            }
            let labels = char_labels(SCORE_LABELS, count);
            Ok((labels.clone(), labels))
        }
        ClassifierQuestion::Bool { .. } => {
            Ok((BOOL_LABELS.map(String::from).to_vec(), vec!["true".into(), "false".into()]))
        }
    }
}

/// The question and its options. `labels` puts the answer labels on choice options.
fn render_task(question: &ClassifierQuestion, labels: Option<&[String]>) -> String {
    let head = format!("Question: {}", question.instructions());
    match question {
        ClassifierQuestion::Choice { criteria, .. } => {
            let lines: Vec<String> = criteria
                .iter()
                .enumerate()
                .map(|(index, (key, description))| {
                    let option = if description.is_empty() { key.to_string() } else { format!("{key}: {description}") };
                    match labels {
                        Some(labels) => format!("{}. {option}", labels[index]),
                        None => format!("- {option}"),
                    }
                })
                .collect();
            format!("{head}\n\nOptions:\n{}", lines.join("\n"))
        }
        ClassifierQuestion::Score { criteria, .. } => {
            let lines: Vec<String> =
                criteria.iter().enumerate().map(|(index, level)| format!("{index}. {level}")).collect();
            format!("{head}\n\nLevels:\n{}", lines.join("\n"))
        }
        ClassifierQuestion::Bool { criteria, .. } => {
            let meanings: Vec<String> = [("Yes", &criteria.when_true), ("No", &criteria.when_false)]
                .into_iter()
                .filter(|(_, meaning)| !meaning.is_empty())
                .map(|(name, meaning)| format!("{name} means: {meaning}"))
                .collect();
            if meanings.is_empty() { head } else { format!("{head}\n\n{}", meanings.join("\n")) }
        }
    }
}

fn answer_instruction(question: &ClassifierQuestion) -> &'static str {
    match question {
        ClassifierQuestion::Choice { .. } => "Answer with one letter.",
        ClassifierQuestion::Score { .. } => "Answer with one level number.",
        ClassifierQuestion::Bool { .. } => "Answer Yes or No.",
    }
}

/// Every question of the request, without answer labels.
fn render_overview(context: &ClassifierContext) -> String {
    let intro = match context.questions.len() {
        1 => "Task: answer the following question about the state.",
        _ => "Task: answer each of the following questions about the state.",
    };
    let tasks = context.questions.values().map(|question| render_task(question, None));
    std::iter::once(intro.to_string()).chain(tasks).collect::<Vec<_>>().join("\n\n")
}

/// One question as a user message: the state, every question, the state again (prompt
/// repetition), then this question with labeled options. Everything before the final question
/// is the same for all questions of a request, so the server's prompt cache evaluates it once.
pub(crate) fn render_question(context: &ClassifierContext, id: &str) -> Result<LabeledQuestion, String> {
    let question = context.questions.get(id).ok_or_else(|| format!("Unknown question: {id}"))?;
    let (labels, keys) = question_labels(question)?;
    let state = render_state(&context.state);
    let last = format!("{}\n\n{}", render_task(question, Some(&labels)), answer_instruction(question));
    let content = [state.clone(), render_overview(context), state, last].join("\n\n");
    Ok(LabeledQuestion { content, labels, keys })
}

/// Softmax over label log-probabilities after dividing them by `temperature`.
pub(crate) fn label_probabilities(logprobs: &[f64], temperature: f64) -> Vec<f64> {
    let scaled: Vec<f64> = logprobs.iter().map(|logprob| logprob / temperature).collect();
    let max = scaled.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let weights: Vec<f64> = scaled.iter().map(|value| (value - max).exp()).collect();
    let total: f64 = weights.iter().fold(0.0, |sum, weight| sum + weight);
    weights.iter().map(|weight| weight / total).collect()
}

/// TypeSafe's documented choice confidence, `(n * peak - 1) / (n - 1)`, clamped to [0, 1].
pub(crate) fn peak_confidence(probabilities: &[f64]) -> f64 {
    let count = probabilities.len() as f64;
    let peak = probabilities.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    ((count * peak - 1.0) / (count - 1.0)).clamp(0.0, 1.0)
}

/// Turn label probabilities, in the order of `keys`, into the public answer shape.
pub(crate) fn answer_from_probabilities(
    question: &ClassifierQuestion,
    keys: &[String],
    probabilities: &[f64],
) -> ClassifierAnswer {
    if let ClassifierQuestion::Bool { .. } = question {
        let index = keys.iter().position(|key| key == "true").unwrap_or_default();
        return ClassifierAnswer::Bool { probability: probabilities[index] };
    }
    let confidence = peak_confidence(probabilities);
    if let ClassifierQuestion::Score { .. } = question {
        let score =
            probabilities.iter().enumerate().fold(0.0, |sum, (index, probability)| sum + index as f64 * probability);
        return ClassifierAnswer::Score { score, confidence };
    }
    let mut best = 0;
    for (index, probability) in probabilities.iter().enumerate().skip(1) {
        if *probability > probabilities[best] {
            best = index;
        }
    }
    let probabilities: OrderedMap<f64> = keys.iter().cloned().zip(probabilities.iter().copied()).collect();
    ClassifierAnswer::Choice { choice: keys[best].clone(), probabilities, confidence }
}
