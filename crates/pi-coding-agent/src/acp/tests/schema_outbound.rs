// The fixture generator runs these cases against schema 0.13.7. The unit test runs them against pi's types.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

fn out(value: impl Serialize) -> String {
    serde_json::to_string(&value).unwrap()
}

fn notify(update: SessionUpdate) -> String {
    out(SessionNotification::new("s1", update))
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(value))
}

fn auth(meta: Option<Map<String, Value>>) -> Vec<AuthMethod> {
    let method = AuthMethodTerminal::new("pi_terminal_login", "Launch pi in the terminal")
        .description("Start pi in an interactive terminal to configure API keys or login")
        .args(vec!["--terminal-login".into()])
        .env(HashMap::new())
        .meta(meta);
    vec![AuthMethod::Terminal(method)]
}

fn initialize(embedded: bool, meta: Option<Map<String, Value>>) -> String {
    let prompt = PromptCapabilities::new().image(true).audio(false).embedded_context(embedded);
    let sessions = SessionCapabilities::new()
        .list(SessionListCapabilities::new())
        .delete(SessionDeleteCapabilities::new())
        .close(SessionCloseCapabilities::new());
    let capabilities = AgentCapabilities::new()
        .load_session(true)
        .mcp_capabilities(McpCapabilities::new().http(true).sse(false))
        .prompt_capabilities(prompt)
        .session_capabilities(sessions);
    out(InitializeResponse::new(ProtocolVersion::V1)
        .agent_info(Implementation::new("pi", "1.2.3").title("pi"))
        .auth_methods(auth(meta))
        .agent_capabilities(capabilities)
        .meta(json!({"piAcp": {"exactMcpPreapproval": true}}).as_object().cloned()))
}

fn select_option(id: &'static str, name: &str, description: Option<String>, meta: Option<Map<String, Value>>) -> SessionConfigSelectOption {
    SessionConfigSelectOption::new(id, name).description(description).meta(meta)
}

fn config_options() -> Vec<SessionConfigOption> {
    let model_meta = object(json!({"supportsReasoningEffort": true, "reasoningEfforts": [{"id": "high", "label": "High", "default": true}]}));
    vec![
        SessionConfigOption::select("model", "Model", "anthropic/opus", vec![
            select_option("anthropic/opus", "Anthropic: Opus", None, Some(model_meta)),
            select_option("openai/gpt-fast", "OpenAI: GPT ⚡", None, None),
        ])
        .category(SessionConfigOptionCategory::Model)
        .description("Select the model for this session"),
        SessionConfigOption::select("thought_level", "Thinking", "high".to_string(), vec![select_option("off", "Off", None, None), select_option("high", "High", None, None)])
            .category(SessionConfigOptionCategory::ThoughtLevel)
            .description("Set the reasoning effort for this session"),
        SessionConfigOption::select("mode", "Mode", "auto", vec![select_option("auto", "Auto", Some("Approve read-only tools".into()), None)])
            .category(SessionConfigOptionCategory::Mode)
            .description("Control tool approval for this session"),
        SessionConfigOption::select("fast_mode", "Fast mode", "standard", vec![select_option("standard", "Fast off", None, None)])
            .category(SessionConfigOptionCategory::Other("_speed".into()))
            .description("Run the model in its fast mode"),
    ]
}

fn modes() -> SessionModeState {
    SessionModeState::new("ask", vec![
        SessionMode::new("auto", "Auto").description("Approve read-only tools; ask before other tools"),
        SessionMode::new("ask", "Ask").description(None::<String>),
    ])
}

fn tool_meta(key: &str) -> Map<String, Value> {
    object(json!({key: {"terminal_id": "t1", "cwd": "/w"}}))
}

fn permission(meta: Option<Map<String, Value>>) -> String {
    let fields = ToolCallUpdateFields::new()
        .title("bash".to_string())
        .kind(ToolKind::Execute)
        .status(ToolCallStatus::Pending)
        .raw_input(json!({"command": "ls"}));
    out(RequestPermissionRequest::new("s1".to_string(), ToolCallUpdate::new("t1", fields).meta(meta), vec![
        PermissionOption::new("allow", "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new("always", "Always", PermissionOptionKind::AllowAlways),
        PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
        PermissionOption::new("never", "Never", PermissionOptionKind::RejectAlways),
    ]))
}

pub fn outbound() -> Vec<(&'static str, String)> {
    let mut usage = Usage::new(30, 10, 20);
    usage.thought_tokens = Some(5);
    usage.cached_read_tokens = Some(0);
    let terminal = ToolCallContent::Terminal(Terminal::new("t1".to_string()));
    vec![
        ("initialize", initialize(true, Some(object(json!({"terminal-auth": {"command": "pi", "args": ["--terminal-login"], "label": "Launch pi"}}))))),
        ("initialize_plain", initialize(false, None)),
        ("auth_methods", out(auth(None))),
        ("authenticate", out(AuthenticateResponse::new())),
        ("close", out(CloseSessionResponse::new())),
        ("delete", out(DeleteSessionResponse::new())),
        ("set_mode", out(SetSessionModeResponse::new())),
        ("list_sessions", out(ListSessionsResponse::new(vec![
            SessionInfo::new("s1".to_string(), PathBuf::from("/w")).title(Some("Title".to_string())).updated_at(Some("2026-11-22T00:00:00Z".to_string())),
            SessionInfo::new("s2".to_string(), PathBuf::from("/w")).title(None::<String>).updated_at(None::<String>),
        ]).next_cursor(Some("50".to_string())).meta(Map::new()))),
        ("list_sessions_last_page", out(ListSessionsResponse::new(Vec::new()).next_cursor(None::<String>))),
        ("available_commands", notify(SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
            AvailableCommand::new("compact", "Manually compact").input(AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("optional custom instructions"))),
            AvailableCommand::new("export", "Export session"),
        ])))),
        ("user_chunk", notify(SessionUpdate::UserMessageChunk(ContentChunk::new(text("hi"))))),
        ("agent_chunk", notify(SessionUpdate::AgentMessageChunk(ContentChunk::new(text("hello")).message_id(MessageId::new("m1".to_string()))))),
        ("thought_chunk", notify(SessionUpdate::AgentThoughtChunk(ContentChunk::new(text("hmm")).message_id(MessageId::new("m1"))))),
        ("notify_chunk", notify(SessionUpdate::AgentMessageChunk(ContentChunk::new(text("note")).meta(object(json!({"piAcp": {"notify": {"level": "warning"}}})))))),
        ("resource_link_chunk", notify(SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::ResourceLink(
            ResourceLink::new("pi-session.html", "file:///w/pi-session.html").mime_type("text/html").title("Session exported"),
        ))))),
        ("tool_call_defaults", notify(SessionUpdate::ToolCall(ToolCall::new("t1".to_string(), "lookup").kind(ToolKind::Other).status(ToolCallStatus::Pending).raw_input(None::<Value>)))),
        ("tool_call_null_input", notify(SessionUpdate::ToolCall(ToolCall::new("t1", "read").kind(ToolKind::Read).status(ToolCallStatus::InProgress).raw_input(Value::Null)
            .locations(vec![ToolCallLocation::new(PathBuf::from("/w/a.rs")).line(Some(3)), ToolCallLocation::new("/w/b.rs").line(None::<u32>)])))),
        ("tool_call_bash", notify(SessionUpdate::ToolCall(ToolCall::new("t1", "ls").kind(ToolKind::Execute).status(ToolCallStatus::Completed)
            .content(vec![terminal.clone()]).raw_input(Some(json!({"command": "ls"}))).raw_output(json!({"output": ""})).meta(tool_meta("terminal_info"))))),
        ("tool_kinds", out(vec![ToolKind::Read, ToolKind::Edit, ToolKind::Delete, ToolKind::Move, ToolKind::Search, ToolKind::Execute, ToolKind::Think, ToolKind::Fetch, ToolKind::SwitchMode, ToolKind::Other])),
        ("tool_update_bash", notify(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new("t1".to_string(), ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed).raw_output(json!({"output": "x", "exitCode": 1}))
            .content(vec![terminal.clone(), ToolCallContent::from(text("x"))])).meta(Map::new())))),
        ("tool_update_all", notify(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new("t2", ToolCallUpdateFields::new()
            .title("edit".to_string()).kind(ToolKind::Other).status(ToolCallStatus::Pending).locations(Some(Vec::new()))
            .raw_input(None::<Value>).content(None::<Vec<ToolCallContent>>))))),
        ("tool_update_diff", notify(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new("t3", ToolCallUpdateFields::new().status(ToolCallStatus::Completed).content(vec![
            ToolCallContent::Diff(Diff::new("/w/a.rs".to_string(), "new").old_text(Some("old".to_string()))),
            ToolCallContent::Diff(Diff::new("/w/b.rs", "created").old_text(None::<String>)),
        ]))))),
        ("config_update", notify(SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(config_options())))),
        ("config_response", out(SetSessionConfigOptionResponse::new(config_options()))),
        ("mode_update", notify(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(modes().current_mode_id)))),
        ("modes", out(modes())),
        ("session_info_title", notify(SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(Some("Name".to_string())).updated_at("2026-11-22T00:00:00Z".to_string())))),
        ("session_info_cleared", notify(SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(None::<String>)))),
        ("session_info_meta", notify(SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().meta(object(json!({"piAcp": {"queueDepth": 1, "running": true}})))))),
        ("usage_update", notify(SessionUpdate::UsageUpdate(UsageUpdate::new(1200, 200_000).cost(Some(Cost::new(0.25, "USD")))))),
        ("usage_update_free", notify(SessionUpdate::UsageUpdate(UsageUpdate::new(0, 1).cost(None::<Cost>)))),
        ("prompt_usage", out(PromptResponse::new(StopReason::EndTurn).usage(Some(usage)))),
        ("prompt_no_usage", out(PromptResponse::new(StopReason::Cancelled).usage(None::<Usage>))),
        ("stop_reasons", out(vec![StopReason::EndTurn, StopReason::MaxTokens, StopReason::MaxTurnRequests, StopReason::Refusal, StopReason::Cancelled])),
        ("permission", permission(None)),
        ("permission_origin", permission(Some(object(json!({"piAcp": {"mcp": {"server": "fs", "tool": "read"}, "parentToolCallId": "c1"}}))))),
        ("error_invalid_params", out(Error::new(-32602, "Invalid params: bad").data(json!("bad")))),
        ("error_internal", out(Error::internal_error())),
        ("error_closed", out(Error::internal_error().data(json!("ACP connection closed")))),
        ("error_auth", out(Error::new(-32000, "Authentication required").data(json!({"authMethods": serde_json::to_value(auth(None)).unwrap()})))),
        ("error_no_data", out(Error::new(-32603, "The prompt failed"))),
    ]
}
