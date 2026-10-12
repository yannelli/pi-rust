//! Coding agent message roles carried as `pi_ai::Message::Custom`: `bashExecution`,
//! `custom`, `branchSummary`, and `compactionSummary`. [`convert_to_llm`] maps them to
//! provider messages exactly as upstream `core/messages.ts` does.

use pi_ai::{ContentBlock, Message, UserContent, UserMessage};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::session::ids::parse_iso;

pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// Integer milliseconds; `null`, fractions, and missing values read as 0.
pub(crate) fn lenient_millis<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|float| float as i64))).unwrap_or(0))
}

/// Typed view of one app-defined role stored in `Message::Custom`.
pub trait CustomRole: Serialize + DeserializeOwned {
    const ROLE: &'static str;

    /// Parse `message` when it carries this role.
    fn from_message(message: &Message) -> Option<Self> {
        match message {
            Message::Custom(custom) if custom.role == Self::ROLE => {
                serde_json::from_value(Value::Object(custom.fields.clone())).ok()
            }
            _ => None,
        }
    }

    fn into_message(self) -> Message {
        let fields = match serde_json::to_value(&self) {
            Ok(Value::Object(fields)) => fields,
            _ => serde_json::Map::new(),
        };
        Message::Custom(pi_ai::CustomMessage::new(Self::ROLE, fields))
    }
}

/// A shell command run with `!` (sent to the model) or `!!` (`exclude_from_context`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    #[serde(default, deserialize_with = "lenient_millis")]
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
}

impl CustomRole for BashExecutionMessage {
    const ROLE: &'static str = "bashExecution";
}

/// An extension-injected message. `display` only controls rendering; the model always sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    #[serde(default)]
    pub custom_type: String,
    #[serde(default = "empty_content")]
    pub content: UserContent,
    #[serde(default)]
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, deserialize_with = "lenient_millis")]
    pub timestamp: i64,
}

fn empty_content() -> UserContent {
    UserContent::Blocks(Vec::new())
}

impl CustomRole for CustomMessage {
    const ROLE: &'static str = "custom";
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryMessage {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub from_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_millis")]
    pub timestamp: i64,
}

impl CustomRole for BranchSummaryMessage {
    const ROLE: &'static str = "branchSummary";
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryMessage {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub tokens_before: u64,
    #[serde(default, deserialize_with = "lenient_millis")]
    pub timestamp: i64,
}

impl CustomRole for CompactionSummaryMessage {
    const ROLE: &'static str = "compactionSummary";
}

/// The user-message text the model sees for a bash execution.
pub fn bash_execution_to_text(message: &BashExecutionMessage) -> String {
    let mut text = format!("Ran `{}`\n", message.command);
    if message.output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str(&format!("```\n{}\n```", message.output));
    }
    if message.cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(code) = message.exit_code.filter(|code| *code != 0) {
        text.push_str(&format!("\n\nCommand exited with code {code}"));
    }
    if message.truncated
        && let Some(path) = message.full_output_path.as_deref().filter(|path| !path.is_empty())
    {
        text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
    }
    text
}

fn iso_millis(timestamp: &str) -> i64 {
    parse_iso(timestamp).unwrap_or_default()
}

pub fn create_branch_summary_message(summary: &str, from_id: &str, timestamp: &str) -> Message {
    BranchSummaryMessage {
        summary: summary.to_string(),
        from_id: Some(from_id.to_string()),
        timestamp: iso_millis(timestamp),
    }
    .into_message()
}

pub fn create_compaction_summary_message(summary: &str, tokens_before: u64, timestamp: &str) -> Message {
    CompactionSummaryMessage { summary: summary.to_string(), tokens_before, timestamp: iso_millis(timestamp) }
        .into_message()
}

/// The `custom` role message for a `custom_message` entry.
pub fn create_custom_message(
    custom_type: &str,
    content: UserContent,
    display: bool,
    details: Option<Value>,
    timestamp: &str,
) -> Message {
    CustomMessage { custom_type: custom_type.to_string(), content, display, details, timestamp: iso_millis(timestamp) }
        .into_message()
}

fn user_text(text: String, timestamp: i64) -> Message {
    Message::User(UserMessage { content: UserContent::Blocks(vec![ContentBlock::text(text)]), timestamp })
}

/// Map coding agent roles to provider messages. Install it as the agent's `convert_to_llm`
/// hook. Excluded bash executions and unknown custom roles produce no message.
pub fn convert_to_llm(messages: &[Message]) -> Vec<Message> {
    messages.iter().filter_map(convert_message).collect()
}

fn convert_message(message: &Message) -> Option<Message> {
    let Message::Custom(custom) = message else { return Some(message.clone()) };
    match custom.role.as_str() {
        BashExecutionMessage::ROLE => {
            let bash = BashExecutionMessage::from_message(message)?;
            if bash.exclude_from_context == Some(true) {
                return None;
            }
            Some(user_text(bash_execution_to_text(&bash), bash.timestamp))
        }
        CustomMessage::ROLE => {
            let custom = CustomMessage::from_message(message)?;
            let content = match custom.content {
                UserContent::Text(text) => UserContent::Blocks(vec![ContentBlock::text(text)]),
                blocks => blocks,
            };
            Some(Message::User(UserMessage { content, timestamp: custom.timestamp }))
        }
        BranchSummaryMessage::ROLE => {
            let summary = BranchSummaryMessage::from_message(message)?;
            Some(user_text(
                format!("{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}", summary.summary),
                summary.timestamp,
            ))
        }
        CompactionSummaryMessage::ROLE => {
            let summary = CompactionSummaryMessage::from_message(message)?;
            let text = format!("{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}", summary.summary);
            Some(user_text(text, summary.timestamp))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn text_of(message: &Message) -> String {
        match message {
            Message::User(user) => user.content.text(),
            _ => panic!("expected a user message"),
        }
    }

    #[test]
    fn converts_bash_executions_and_honors_exclusion() {
        let bash = BashExecutionMessage {
            command: "ls".into(),
            output: "a\nb".into(),
            exit_code: Some(2),
            truncated: true,
            full_output_path: Some("/tmp/out".into()),
            timestamp: 5,
            ..Default::default()
        };
        let converted = convert_to_llm(&[bash.clone().into_message()]);
        assert_eq!(
            text_of(&converted[0]),
            "Ran `ls`\n```\na\nb\n```\n\nCommand exited with code 2\n\n[Output truncated. Full output: /tmp/out]"
        );
        let cancelled = BashExecutionMessage { output: String::new(), cancelled: true, ..bash.clone() };
        assert_eq!(
            bash_execution_to_text(&cancelled),
            "Ran `ls`\n(no output)\n\n(command cancelled)\n\n[Output truncated. Full output: /tmp/out]"
        );
        let excluded = BashExecutionMessage { exclude_from_context: Some(true), ..bash };
        assert!(convert_to_llm(&[excluded.into_message()]).is_empty());
    }

    #[test]
    fn converts_custom_and_summary_roles() {
        let custom =
            create_custom_message("ext", UserContent::Text("hello".into()), false, None, "1970-01-01T00:00:01Z");
        let branch = create_branch_summary_message("left work", "abc", "1970-01-01T00:00:02Z");
        let compaction = create_compaction_summary_message("history", 10, "1970-01-01T00:00:03Z");
        let unknown = Message::Custom(pi_ai::CustomMessage::new("other", serde_json::Map::new()));
        let converted = convert_to_llm(&[custom, branch.clone(), compaction, unknown, Message::user("plain")]);
        assert_eq!(converted.len(), 4);
        assert!(matches!(&converted[0], Message::User(user) if user.timestamp == 1000
            && matches!(&user.content, UserContent::Blocks(blocks) if blocks.len() == 1)));
        assert_eq!(text_of(&converted[1]), format!("{BRANCH_SUMMARY_PREFIX}left work{BRANCH_SUMMARY_SUFFIX}"));
        assert_eq!(text_of(&converted[2]), format!("{COMPACTION_SUMMARY_PREFIX}history{COMPACTION_SUMMARY_SUFFIX}"));
        assert_eq!(
            serde_json::to_value(&branch).unwrap(),
            json!({"role": "branchSummary", "summary": "left work", "fromId": "abc", "timestamp": 2000})
        );
    }

    #[test]
    fn bash_execution_field_order_matches_upstream() {
        let message: Message = serde_json::from_value(json!({"role": "bashExecution", "command": "pwd",
            "output": "/w", "exitCode": 0, "cancelled": false, "truncated": false, "timestamp": 9}))
        .unwrap();
        let typed = BashExecutionMessage::from_message(&message).unwrap();
        assert_eq!(typed.exit_code, Some(0));
        let text = serde_json::to_string(&typed.into_message()).unwrap();
        assert_eq!(
            text,
            r#"{"role":"bashExecution","command":"pwd","output":"/w","exitCode":0,"cancelled":false,"truncated":false,"timestamp":9}"#
        );
    }
}
