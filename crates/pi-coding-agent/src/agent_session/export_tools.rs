//! Tool renderers for HTML export (upstream `exportToHtml` with `createToolHtmlRenderer`): calls
//! and results of tools with TUI renderers are drawn as ANSI lines and embedded in the page.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use pi_tui::Component;
use serde_json::{Value, json};

use super::AgentSession;
use crate::export::AnsiToolRenderer;
use crate::extensions::{ExtensionRunner, ToolRenderRequest};
use crate::interactive::renderers::{
    McpRenderer, RendererState, ToolRenderContext, ToolRenderer, ToolResultView, builtin_renderer,
};

/// Renders the session's tools: extension renderers first, then the tool's own renderer.
pub(super) struct SessionToolRenderer {
    runner: Option<Arc<ExtensionRunner>>,
    /// `(tool name, label)` of the registered MCP tools.
    mcp_labels: HashMap<String, String>,
    cwd: PathBuf,
    args: RefCell<HashMap<String, Value>>,
    states: RefCell<HashMap<String, RendererState>>,
}

impl SessionToolRenderer {
    pub(super) fn new(session: &AgentSession) -> Self {
        let mcp_labels = session
            .all_tools()
            .into_iter()
            .filter(|tool| tool.source_path == "builtin:mcp")
            .filter_map(|tool| session.tool(&tool.name).map(|shared| (tool.name, shared.label().to_string())))
            .collect();
        Self {
            runner: session.extension_runner(),
            mcp_labels,
            cwd: session.cwd().to_path_buf(),
            args: RefCell::default(),
            states: RefCell::default(),
        }
    }

    fn own_renderer(&self, tool_name: &str) -> Option<Rc<dyn ToolRenderer>> {
        if let Some(renderer) = builtin_renderer(tool_name) {
            return Some(renderer);
        }
        let mcp = match self.mcp_labels.get(tool_name) {
            Some(label) => Some(McpRenderer { label: label.clone() }),
            None => McpRenderer::from_tool_name(tool_name),
        };
        mcp.map(|renderer| Rc::new(renderer) as Rc<dyn ToolRenderer>)
    }

    fn request(
        &self,
        tool_call_id: &str,
        args: &Value,
        partial: bool,
        expanded: bool,
        error: bool,
    ) -> ToolRenderRequest {
        ToolRenderRequest {
            tool_call_id: tool_call_id.to_string(),
            args: args.clone(),
            cwd: self.cwd.display().to_string(),
            execution_started: true,
            args_complete: true,
            is_partial: partial,
            expanded,
            show_images: false,
            is_error: error,
            duration_ms: None,
            output_pad: 1,
        }
    }

    fn render_own(
        &self,
        renderer: &dyn ToolRenderer,
        request: &ToolRenderRequest,
        result: Option<&ToolResultView>,
        width: usize,
    ) -> Option<Vec<String>> {
        let mut states = self.states.borrow_mut();
        let state = states.entry(request.tool_call_id.clone()).or_default();
        let mut context = ToolRenderContext {
            args: &request.args,
            tool_call_id: &request.tool_call_id,
            invalidate: Rc::new(|| {}),
            state,
            cwd: &self.cwd,
            execution_started: true,
            args_complete: true,
            is_partial: request.is_partial,
            expanded: request.expanded,
            show_images: false,
            is_error: request.is_error,
            duration_ms: None,
            output_pad: 1,
        };
        let component = match result {
            None => renderer.render_call(&mut context),
            Some(result) => renderer.render_result(result, &mut context)?,
        };
        let lines = component.borrow_mut().render(width);
        Some(lines)
    }
}

impl AnsiToolRenderer for SessionToolRenderer {
    fn render_call_lines(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        args: &Value,
        width: usize,
    ) -> Option<Vec<String>> {
        self.args.borrow_mut().insert(tool_call_id.to_string(), args.clone());
        let request = self.request(tool_call_id, args, true, false, false);
        let info = self.runner.as_ref().and_then(|runner| runner.tool_renderer_info(tool_name));
        if let (Some(runner), Some(info)) = (&self.runner, info)
            && info.call
        {
            let mut component = runner.render_tool_call(tool_name, &request)?;
            return Some(component.render(width));
        }
        let renderer = self.own_renderer(tool_name)?;
        self.render_own(renderer.as_ref(), &request, None, width)
    }

    fn render_result_lines(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        content: &[Value],
        details: &Value,
        is_error: bool,
        expanded: bool,
        width: usize,
    ) -> Option<Vec<String>> {
        let args = self.args.borrow().get(tool_call_id).cloned().unwrap_or(Value::Null);
        let request = self.request(tool_call_id, &args, false, expanded, is_error);
        let info = self.runner.as_ref().and_then(|runner| runner.tool_renderer_info(tool_name));
        if let (Some(runner), Some(info)) = (&self.runner, info)
            && info.result
        {
            let value = json!({"content": content, "details": details, "isError": is_error});
            let mut component = runner.render_tool_result(tool_name, &value, &request)?;
            return Some(component.render(width));
        }
        let renderer = self.own_renderer(tool_name)?;
        let result = ToolResultView {
            content: serde_json::from_value(Value::Array(content.to_vec())).unwrap_or_default(),
            details: (!details.is_null()).then(|| details.clone()),
            is_error,
            duration_ms: None,
        };
        Some(self.render_own(renderer.as_ref(), &request, Some(&result), width).unwrap_or_default())
    }
}
