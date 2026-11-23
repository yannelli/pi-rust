// Inbound parity cases: what parses, what each parse keeps, and the serde error text otherwise.
// Included after `schema_outbound.rs`, whose imports and helpers it uses.

fn parse<T: DeserializeOwned>(value: Value, project: impl Fn(T) -> String) -> String {
    match serde_json::from_value::<T>(value) {
        Ok(parsed) => format!("ok {}", project(parsed)),
        Err(error) => format!("err {error}"),
    }
}

fn server(server: &McpServer) -> String {
    if let McpServer::Http(http) = server {
        let headers: Vec<_> = http.headers.iter().map(|h| (&h.name, &h.value, &h.meta)).collect();
        format!("http {} {} {headers:?} {:?}", http.name, http.url, http.meta)
    } else if let McpServer::Sse(sse) = server {
        format!("sse {} {} {}", sse.name, sse.url, sse.headers.len())
    } else if let McpServer::Stdio(stdio) = server {
        let env: Vec<_> = stdio.env.iter().map(|e| (&e.name, &e.value, &e.meta)).collect();
        format!("stdio {} {:?} {:?} {env:?} {:?}", stdio.name, stdio.command, stdio.args, stdio.meta)
    } else {
        "other".into()
    }
}

fn servers(list: &[McpServer]) -> String {
    list.iter().map(server).collect::<Vec<_>>().join("; ")
}

fn initialize_params(request: InitializeRequest) -> String {
    let caps = &request.client_capabilities;
    let info = request.client_info.map(|info| (info.name, info.title, info.version, info.meta));
    format!(
        "v={} fs={},{},{:?} terminal={} auth={},{:?} capsMeta={:?} info={info:?} meta={:?}",
        out(request.protocol_version),
        caps.fs.read_text_file,
        caps.fs.write_text_file,
        caps.fs.meta,
        caps.terminal,
        caps.auth.terminal,
        caps.auth.meta,
        caps.meta,
        request.meta,
    )
}

fn new_session(request: NewSessionRequest) -> String {
    format!("{:?} {:?} [{}] {:?}", request.cwd, request.additional_directories, servers(&request.mcp_servers), request.meta)
}

fn prompt(request: PromptRequest) -> String {
    format!("{} {} {:?}", request.session_id, out(&request.prompt), request.meta)
}

fn permission_response(response: RequestPermissionResponse) -> String {
    let outcome = if let RequestPermissionOutcome::Selected(selected) = &response.outcome {
        format!("selected {} {:?}", selected.option_id, selected.meta)
    } else if matches!(response.outcome, RequestPermissionOutcome::Cancelled) {
        "cancelled".into()
    } else {
        "other".into()
    };
    format!("{outcome} {:?}", response.meta)
}

fn error(error: Error) -> String {
    format!("{} {} {:?}", out(error.code), error.message, error.data)
}

fn prompt_of(blocks: Value) -> Value {
    json!({"sessionId": "s1", "prompt": blocks})
}

fn list(request: ListSessionsRequest) -> String {
    format!("{:?} {:?} {:?}", request.cwd, request.cursor, request.meta)
}

pub fn inbound() -> Vec<(&'static str, String)> {
    let stdio = json!({"name": "fs", "command": "/bin/fs", "args": ["-v"], "env": [{"name": "A", "value": "1", "_meta": {"k": 1}}]});
    let http = json!({"type": "http", "name": "web", "url": "https://x", "headers": [{"name": "H", "value": "v"}], "_meta": {"m": true}});
    let new = |servers: Value| json!({"cwd": "/w", "mcpServers": servers});
    let resource = json!({"type": "resource_link", "name": "a.rs", "uri": "file:///a.rs", "description": "d", "mimeType": "text/x", "size": 12, "title": "A", "_meta": {"x": 1}});
    let annotations = json!({"audience": ["user", "robot", 5, "assistant"], "lastModified": "2026-01-01", "priority": 0.5, "_meta": {"a": 1}});
    vec![
        ("initialize_v1", parse(json!({"protocolVersion": 1}), initialize_params)),
        ("initialize_string", parse(json!({"protocolVersion": "1.0.0"}), initialize_params)),
        ("initialize_too_large", parse(json!({"protocolVersion": 70000}), initialize_params)),
        ("initialize_negative", parse(json!({"protocolVersion": -1}), initialize_params)),
        ("initialize_float", parse(json!({"protocolVersion": 1.5}), initialize_params)),
        ("initialize_missing", parse(json!({}), initialize_params)),
        ("initialize_zed", parse(json!({"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": true, "_meta": {"f": 1}}, "terminal": true,
            "auth": {"terminal": true}, "_meta": {"terminal-auth": true, "terminal_output": true}}, "clientInfo": {"name": "zed", "title": "Zed", "version": "1"}, "_meta": {"r": 1}}), initialize_params)),
        ("initialize_bad_fs", parse(json!({"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": "yes"}}}), initialize_params)),
        ("initialize_bad_terminal", parse(json!({"protocolVersion": 1, "clientCapabilities": {"terminal": "no"}}), initialize_params)),
        ("initialize_bad_auth", parse(json!({"protocolVersion": 1, "clientCapabilities": {"auth": {"terminal": 1}}}), initialize_params)),
        ("initialize_null_caps", parse(json!({"protocolVersion": 1, "clientCapabilities": null}), initialize_params)),
        ("initialize_bad_caps_meta", parse(json!({"protocolVersion": 1, "clientCapabilities": {"_meta": 5}}), initialize_params)),
        ("initialize_bad_info", parse(json!({"protocolVersion": 1, "clientInfo": {"name": 5}}), initialize_params)),
        ("initialize_bad_meta", parse(json!({"protocolVersion": 1, "_meta": 5}), initialize_params)),
        ("initialize_null_meta", parse(json!({"protocolVersion": 1, "_meta": null}), initialize_params)),
        ("new_session", parse(new(json!([stdio, http])), new_session)),
        ("new_session_meta", parse(json!({"cwd": "/w", "mcpServers": [], "additionalDirectories": ["/x"], "_meta": {"piAcp": {"clientMcpExposure": "direct"}}}), new_session)),
        ("new_session_no_servers", parse(json!({"cwd": "/w"}), new_session)),
        ("new_session_no_cwd", parse(json!({"mcpServers": []}), new_session)),
        ("new_session_bad_dirs", parse(json!({"cwd": "/w", "mcpServers": [], "additionalDirectories": "/x"}), new_session)),
        ("mcp_typed_stdio", parse(new(json!([{"type": "stdio", "name": "a", "command": "/a", "args": [], "env": []}])), new_session)),
        ("mcp_sse", parse(new(json!([{"type": "sse", "name": "s", "url": "https://s", "headers": []}])), new_session)),
        ("mcp_unknown_type", parse(new(json!([{"type": "acp", "name": "a", "id": "x"}])), new_session)),
        ("mcp_http_no_url", parse(new(json!([{"type": "http", "name": "w", "headers": []}])), new_session)),
        ("mcp_http_no_headers", parse(new(json!([{"type": "http", "name": "w", "url": "https://x"}])), new_session)),
        ("mcp_stdio_no_env", parse(new(json!([{"name": "a", "command": "/a", "args": []}])), new_session)),
        ("mcp_bad_env", parse(new(json!([{"name": "a", "command": "/a", "args": [], "env": [{"name": "A"}]}])), new_session)),
        ("load_session", parse(json!({"sessionId": "s1", "cwd": "/w", "mcpServers": [http]}), |r: LoadSessionRequest| format!("{} {:?} [{}]", r.session_id, r.cwd, servers(&r.mcp_servers)))),
        ("load_session_no_id", parse(json!({"cwd": "/w", "mcpServers": []}), |r: LoadSessionRequest| r.session_id.to_string())),
        ("list_empty", parse(json!({}), list)),
        ("list_full", parse(json!({"cwd": "/w", "cursor": "50", "_meta": {}}), list)),
        ("list_bad_cursor", parse(json!({"cursor": 5}), list)),
        ("delete_params", parse(json!({"sessionId": "s1", "_meta": {"x": 1}}), |r: DeleteSessionRequest| format!("{} {:?}", r.session_id, r.meta))),
        ("close_missing", parse(json!({}), |r: CloseSessionRequest| r.session_id.to_string())),
        ("cancel_number", parse(json!({"sessionId": 5}), |r: CancelNotification| r.session_id.to_string())),
        ("cancel", parse(json!({"sessionId": "s1"}), |r: CancelNotification| r.session_id.to_string())),
        ("prompt_text", parse(prompt_of(json!([{"type": "text", "text": "hi", "_meta": {"t": 1}}])), prompt)),
        ("prompt_media", parse(prompt_of(json!([{"type": "image", "data": "aGk=", "mimeType": "image/png", "uri": "file:///i.png"},
            {"type": "audio", "data": "aGk=", "mimeType": "audio/wav"}])), prompt)),
        ("prompt_resource_link", parse(prompt_of(json!([resource])), prompt)),
        ("prompt_resources", parse(prompt_of(json!([{"type": "resource", "resource": {"uri": "file:///a", "text": "x", "mimeType": "text/plain"}},
            {"type": "resource", "resource": {"uri": "file:///b", "blob": "aGk="}}])), prompt)),
        ("prompt_resource_neither", parse(prompt_of(json!([{"type": "resource", "resource": {"uri": "file:///a"}}])), prompt)),
        ("prompt_unknown_block", parse(prompt_of(json!([{"type": "video", "data": "x"}])), prompt)),
        ("prompt_text_missing", parse(prompt_of(json!([{"type": "text"}])), prompt)),
        ("prompt_bad_image_uri", parse(prompt_of(json!([{"type": "image", "data": "x", "mimeType": "image/png", "uri": 5}])), prompt)),
        ("prompt_annotations", parse(prompt_of(json!([{"type": "text", "text": "a", "annotations": annotations},
            {"type": "image", "data": "x", "mimeType": "image/png", "annotations": {"priority": 1.0}},
            {"type": "audio", "data": "x", "mimeType": "audio/wav", "annotations": {"audience": "user"}},
            {"type": "resource", "resource": {"uri": "file:///a", "text": "x"}, "annotations": {"audience": null, "lastModified": "d"}},
            {"type": "resource_link", "name": "n", "uri": "u", "annotations": {}}])), prompt)),
        ("prompt_bad_annotations", parse(prompt_of(json!([{"type": "text", "text": "a", "annotations": 5},
            {"type": "text", "text": "b", "annotations": {"audience": ["user"], "lastModified": 5}},
            {"type": "text", "text": "c", "annotations": {"priority": "high"}},
            {"type": "text", "text": "d", "annotations": null}])), prompt)),
        ("prompt_no_session", parse(json!({"prompt": []}), prompt)),
        ("set_mode_params", parse(json!({"sessionId": "s1", "modeId": "ask"}), |r: SetSessionModeRequest| format!("{} {}", r.session_id, r.mode_id))),
        ("set_mode_missing", parse(json!({"sessionId": "s1"}), |r: SetSessionModeRequest| r.mode_id.to_string())),
        ("set_config", parse(json!({"sessionId": "s1", "configId": "model", "value": "a/b"}), |r: SetSessionConfigOptionRequest| format!("{} {} {}", r.session_id, r.config_id, r.value))),
        ("set_config_bool", parse(json!({"sessionId": "s1", "configId": "x", "type": "boolean", "value": true}), |r: SetSessionConfigOptionRequest| r.value.to_string())),
        ("set_config_number", parse(json!({"sessionId": "s1", "configId": "x", "value": 5}), |r: SetSessionConfigOptionRequest| r.value.to_string())),
        ("permission_selected", parse(json!({"outcome": {"outcome": "selected", "optionId": "allow", "_meta": {"s": 1}}, "_meta": {"r": 1}}), permission_response)),
        ("permission_cancelled", parse(json!({"outcome": {"outcome": "cancelled", "optionId": "allow"}}), permission_response)),
        ("permission_snake_case", parse(json!({"outcome": {"outcome": "selected", "option_id": "allow"}}), permission_response)),
        ("permission_unknown", parse(json!({"outcome": {"outcome": "approved"}}), permission_response)),
        ("permission_flat", parse(json!({"outcome": "selected", "optionId": "allow"}), permission_response)),
        ("permission_missing", parse(json!({}), permission_response)),
        ("error_full", parse(json!({"code": -32000, "message": "Auth", "data": {"x": 1}}), error)),
        ("error_null_data", parse(json!({"code": 5, "message": "", "data": null}), error)),
        ("error_float_code", parse(json!({"code": 1.5, "message": "x"}), error)),
        ("error_large_code", parse(json!({"code": 3_000_000_000_i64, "message": "x"}), error)),
        ("error_no_message", parse(json!({"code": -32603}), error)),
        ("error_string_code", parse(json!({"code": "x", "message": "x"}), error)),
    ]
}
