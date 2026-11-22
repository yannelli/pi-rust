//! The edit tool renderer (upstream `renderers/edit.ts`): the call header shows a diff preview
//! computed from the file once the arguments are complete; the result adds the applied diff when
//! it differs from the preview, or the error.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_tui::components::StyleFn;
use pi_tui::{BoxComponent, ComponentRef, Container, Spacer, Text, component};
use serde_json::Value;
use tokio::sync::oneshot;

use super::{ToolRenderContext, ToolRenderer, ToolResultView, path_arg, render_tool_path};
use crate::interactive::components::diff::render_diff;
use crate::theme::{ThemeBg, ThemeColor, theme};
use crate::tools::edit_diff::{
    DEFAULT_CONTEXT_LINES, Edit, apply_edits_to_normalized_content, generate_diff_string, normalize_to_lf,
};
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
    pending: Option<oneshot::Receiver<EditPreview>>,
    args_key: Option<String>,
    settled_error: bool,
}

fn start_preview(
    path: String,
    edits: Vec<Edit>,
    cwd: PathBuf,
    redraw: Arc<dyn Fn() + Send + Sync>,
) -> oneshot::Receiver<EditPreview> {
    let (send, receive) = oneshot::channel();
    let work = move || {
        if !send.is_closed() {
            let preview = compute_edits_diff(&path, &edits, &cwd);
            if send.send(preview).is_ok() {
                redraw();
            }
        }
    };
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn_blocking(work);
    } else {
        std::thread::spawn(work);
    }
    receive
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
            let (diff, first_changed_line) = generate_diff_string(&base, &new, DEFAULT_CONTEXT_LINES);
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

fn build_call(state: &EditState, path: &str, output_pad: usize) -> ComponentRef {
    let theme = theme();
    let mut header = BoxComponent::new(output_pad, 1, Some(header_bg(state)));
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
        let redraw = context.state.redraw_from_worker.clone();
        let state = context.state.get_or_default::<EditState>();
        if state.args_key != key {
            *state = EditState { args_key: key, ..EditState::default() };
        }
        if let Some(pending) = &mut state.pending {
            match pending.try_recv() {
                Ok(preview) => {
                    state.preview = Some(preview);
                    state.pending = None;
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    state.preview = Some(EditPreview::Error("Edit preview worker stopped.".into()));
                    state.pending = None;
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
        }
        if context.args_complete && state.preview.is_none() && state.pending.is_none() {
            if let Some((path, edits)) = input {
                match redraw {
                    Some(redraw) => state.pending = Some(start_preview(path, edits, context.cwd.to_owned(), redraw)),
                    None => state.preview = Some(compute_edits_diff(&path, &edits, context.cwd)),
                }
            }
        }
        let path = render_tool_path(path_arg(context.args).as_deref(), context.cwd, None);
        build_call(state, &path, context.output_pad)
    }

    fn render_result(&self, result: &ToolResultView, context: &mut ToolRenderContext<'_>) -> Option<ComponentRef> {
        let is_error = context.is_error;
        let result_diff = if is_error { None } else { result.detail("diff").and_then(Value::as_str) };
        let first_changed_line = result.detail("firstChangedLine").and_then(Value::as_u64).map(|line| line as usize);
        let state = context.state.get_or_default::<EditState>();
        if let Some(diff) = result_diff {
            state.pending = None;
            state.preview = Some(EditPreview::Diff { diff: diff.to_string(), first_changed_line });
        }
        state.settled_error = is_error;
        let output = if is_error {
            let text = result.raw_text();
            let preview_error = match &state.preview {
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
