//! The edit tool renderer (upstream `renderers/edit.ts`): the call header shows a diff preview
//! computed from the file once the arguments are complete; the result adds the applied diff when
//! it differs from the preview, or the error.

use std::path::Path;

use pi_tui::components::StyleFn;
use pi_tui::{BoxComponent, ComponentRef, Container, Spacer, Text, component};
use serde_json::Value;

use super::{ToolRenderContext, ToolRenderer, ToolResultView, path_arg, render_tool_path};
use crate::interactive::components::diff::render_diff;
use crate::theme::{ThemeBg, ThemeColor, theme};
use crate::tools::edit_diff::{Edit, apply_edits_to_normalized_content, generate_diff_string, normalize_to_lf};
use crate::tools::path_utils::resolve_to_cwd;

pub struct EditRenderer;

/// A computed diff preview, or why it could not be computed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditPreview {
    Diff { diff: String, first_changed_line: Option<usize> },
    Error(String),
}

#[derive(Default)]
struct EditState {
    preview: Option<EditPreview>,
    args_key: Option<String>,
    settled_error: bool,
}

fn preview_input(args: &Value) -> Option<(String, Vec<Edit>)> {
    let path = ["path", "file_path"].iter().find_map(|key| args.get(*key).and_then(Value::as_str))?;
    if path.is_empty() {
        return None;
    }
    let edit_of = |value: &Value| {
        Some(Edit {
            old_text: value.get("oldText")?.as_str()?.to_string(),
            new_text: value.get("newText")?.as_str()?.to_string(),
        })
    };
    if let Some(edits) = args.get("edits").and_then(Value::as_array).filter(|edits| !edits.is_empty()) {
        let parsed: Option<Vec<Edit>> = edits.iter().map(edit_of).collect();
        if let Some(parsed) = parsed {
            return Some((path.to_string(), parsed));
        }
    }
    edit_of(args).map(|edit| (path.to_string(), vec![edit]))
}

fn args_key(input: &(String, Vec<Edit>)) -> String {
    let edits: Vec<Value> =
        input.1.iter().map(|edit| serde_json::json!({"oldText": edit.old_text, "newText": edit.new_text})).collect();
    serde_json::json!({"path": input.0, "edits": edits}).to_string()
}

fn error_code(error: &std::io::Error) -> String {
    use std::io::ErrorKind;
    let code = match error.kind() {
        ErrorKind::NotFound => "ENOENT",
        ErrorKind::PermissionDenied => "EACCES",
        ErrorKind::IsADirectory => "EISDIR",
        _ => return error.to_string(),
    };
    format!("Error code: {code}")
}

/// Upstream `computeEditsDiff`: the diff the edits would produce, without writing.
pub fn compute_edits_diff(path: &str, edits: &[Edit], cwd: &Path) -> EditPreview {
    let absolute = resolve_to_cwd(path, cwd);
    let raw = match std::fs::read(&absolute) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => return EditPreview::Error(format!("Could not edit file: {path}. {}.", error_code(&error))),
    };
    let content = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
    match apply_edits_to_normalized_content(&normalize_to_lf(content), edits, path) {
        Ok((base, new)) => {
            let (diff, first_changed_line) = generate_diff_string(&base, &new);
            EditPreview::Diff { diff, first_changed_line }
        }
        Err(error) => EditPreview::Error(error.to_string()),
    }
}

fn header_bg(state: &EditState) -> StyleFn {
    let bg = match &state.preview {
        Some(EditPreview::Error(_)) => ThemeBg::ToolErrorBg,
        Some(EditPreview::Diff { .. }) => ThemeBg::ToolSuccessBg,
        None if state.settled_error => ThemeBg::ToolErrorBg,
        None => ThemeBg::ToolPendingBg,
    };
    std::rc::Rc::new(move |text: &str| theme().bg(bg, text))
}

fn build_call(state: &EditState, context: &ToolRenderContext<'_>) -> ComponentRef {
    let theme = theme();
    let mut header = BoxComponent::new(context.output_pad, 1, Some(header_bg(state)));
    let path = render_tool_path(path_arg(context.args).as_deref(), context.cwd, None);
    let title = format!("{} {path}", theme.fg(ThemeColor::ToolTitle, &theme.bold("edit")));
    header.add_child(component(Text::new(&title, 0, 0)));
    if let Some(preview) = &state.preview {
        let body = match preview {
            EditPreview::Error(error) => theme.fg(ThemeColor::Error, error),
            EditPreview::Diff { diff, .. } => render_diff(diff),
        };
        header.add_child(component(Spacer::new(1)));
        header.add_child(component(Text::new(&body, 0, 0)));
    }
    component(header)
}

impl ToolRenderer for EditRenderer {
    fn render_call(&self, context: &mut ToolRenderContext<'_>) -> ComponentRef {
        let input = preview_input(context.args);
        let key = input.as_ref().map(args_key);
        let (args_complete, cwd) = (context.args_complete, context.cwd.to_path_buf());
        let state = context.state.get_or_default::<EditState>();
        if state.args_key != key {
            *state = EditState { args_key: key, ..EditState::default() };
        }
        if args_complete
            && state.preview.is_none()
            && let Some((path, edits)) = &input
        {
            state.preview = Some(compute_edits_diff(path, edits, &cwd));
        }
        let state = context.state.get_or_default::<EditState>();
        let snapshot = EditState { preview: state.preview.clone(), args_key: None, settled_error: state.settled_error };
        build_call(&snapshot, context)
    }

    fn render_result(&self, result: &ToolResultView, context: &mut ToolRenderContext<'_>) -> Option<ComponentRef> {
        let is_error = context.is_error;
        let result_diff = if is_error { None } else { result.detail("diff").and_then(Value::as_str) };
        let first_changed_line = result.detail("firstChangedLine").and_then(Value::as_u64).map(|line| line as usize);
        let state = context.state.get_or_default::<EditState>();
        if let Some(diff) = result_diff {
            state.preview = Some(EditPreview::Diff { diff: diff.to_string(), first_changed_line });
        }
        state.settled_error = is_error;
        let preview = state.preview.clone();
        let output = if is_error {
            let text = result.raw_text();
            let preview_error = match &preview {
                Some(EditPreview::Error(error)) => Some(error.as_str()),
                _ => None,
            };
            (!text.is_empty() && Some(text.as_str()) != preview_error).then(|| theme().fg(ThemeColor::Error, &text))
        } else {
            None
        };
        let mut container = Container::new();
        if let Some(output) = output {
            container.add_child(component(Spacer::new(1)));
            container.add_child(component(Text::new(&output, context.output_pad, 0)));
        }
        Some(component(container))
    }

    fn refresh_call_after_result(&self) -> bool {
        true
    }
}
