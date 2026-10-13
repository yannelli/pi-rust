//! One tool call with its streaming and final result (upstream `components/tool-execution.ts`).
//! Built-in tools draw through their renderers; other known tools get a generic header and text
//! output; unknown tools show their name, JSON arguments, and output.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use pi_ai::ContentBlock;
use pi_tui::components::StyleFn;
use pi_tui::components::image::{ImageOptions, ImageTheme};
use pi_tui::terminal_image::get_capabilities;
use pi_tui::{
    BoxComponent, Component, ComponentRef, Container, Image, MouseRegion, MouseResponse, Spacer, Text, Tui,
    TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType, component,
};
use serde_json::Value;

use crate::interactive::renderers::{
    RendererState, ToolRenderContext, ToolRenderer, ToolResultView, format_tool_call_with_args, get_text_output,
    more_lines_hint, text,
};
use crate::theme::{ThemeBg, ThemeColor, theme};

const FALLBACK_PREVIEW_LINES: usize = 10;

/// Display options from the settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolExecutionOptions {
    pub show_images: bool,
    pub image_width_cells: usize,
    pub output_pad: usize,
}

impl Default for ToolExecutionOptions {
    fn default() -> Self {
        Self { show_images: true, image_width_cells: 60, output_pad: 1 }
    }
}

/// How a tool is drawn.
#[derive(Clone)]
pub enum ToolDisplay {
    /// A built-in renderer.
    Renderer(Rc<dyn ToolRenderer>),
    /// A registered tool without renderers: generic header and text output.
    Generic,
    /// A tool the session does not know.
    Unknown,
}

struct ImageEntry {
    data: String,
    mime_type: String,
    width_cells: usize,
    component: ComponentRef,
}

pub struct ToolExecutionComponent {
    tool_name: String,
    tool_call_id: String,
    args: Value,
    options: ToolExecutionOptions,
    display: ToolDisplay,
    cwd: PathBuf,
    tui: Tui,
    execution_started: bool,
    args_complete: bool,
    is_partial: bool,
    expanded: bool,
    result: Option<ToolResultView>,
    state: RendererState,
    content_box: Rc<RefCell<BoxComponent>>,
    content_text: Rc<RefCell<Text>>,
    images: Vec<ImageEntry>,
    /// Set by the render context's `invalidate`, consumed on the next render.
    dirty: Rc<Cell<bool>>,
    clicked: Rc<Cell<bool>>,
    has_result: Rc<Cell<bool>>,
    /// Weak handle to `dirty` for timers, so they do not keep the component alive.
    self_dirty: Weak<Cell<bool>>,
}

fn bg(token: ThemeBg) -> StyleFn {
    Rc::new(move |text: &str| theme().bg(token, text))
}

impl ToolExecutionComponent {
    pub fn new(
        tool_name: &str,
        tool_call_id: &str,
        args: Value,
        options: ToolExecutionOptions,
        display: ToolDisplay,
        tui: &Tui,
        cwd: PathBuf,
    ) -> Self {
        let dirty = Rc::new(Cell::new(false));
        let self_dirty = Rc::downgrade(&dirty);
        let mut component = Self {
            tool_name: tool_name.to_string(),
            tool_call_id: tool_call_id.to_string(),
            args,
            options,
            display,
            cwd,
            tui: tui.clone(),
            execution_started: false,
            args_complete: false,
            is_partial: true,
            expanded: false,
            result: None,
            state: RendererState::default(),
            content_box: component(BoxComponent::new(1, 1, Some(bg(ThemeBg::ToolPendingBg)))),
            content_text: component(Text::new("", 1, 1).with_custom_bg_fn(Some(bg(ThemeBg::ToolPendingBg)))),
            images: Vec::new(),
            dirty,
            clicked: Rc::default(),
            has_result: Rc::default(),
            self_dirty,
        };
        component.update_display();
        component
    }

    pub fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }

    pub fn update_args(&mut self, args: Value) {
        self.args = args;
        self.update_display();
    }

    pub fn mark_execution_started(&mut self) {
        self.execution_started = true;
        self.update_display();
        self.tui.request_render(false);
    }

    pub fn set_args_complete(&mut self) {
        self.args_complete = true;
        self.update_display();
        self.tui.request_render(false);
    }

    pub fn update_result(&mut self, result: ToolResultView, is_partial: bool) {
        self.result = Some(result);
        self.is_partial = is_partial;
        self.has_result.set(true);
        self.update_display();
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.update_display();
    }

    pub fn set_output_pad(&mut self, padding: usize) {
        self.options.output_pad = padding;
        self.update_display();
    }

    pub fn set_show_images(&mut self, show: bool) {
        self.options.show_images = show;
        self.update_display();
    }

    pub fn set_image_width_cells(&mut self, width: usize) {
        self.options.image_width_cells = width.max(1);
        self.update_display();
    }

    fn region(&self, child: ComponentRef) -> ComponentRef {
        let (clicked, has_result) = (self.clicked.clone(), self.has_result.clone());
        component(MouseRegion::new(child, move |event| {
            if !has_result.get() || event.kind != TuiMouseEventType::Click || event.button != TuiMouseButton::Left {
                return None;
            }
            clicked.set(true);
            Some(TuiMouseEventResult::handled())
        }))
    }

    fn result_fallback(&self) -> Option<ComponentRef> {
        let output = get_text_output(self.result.as_ref()?, self.options.show_images);
        if output.is_empty() {
            return None;
        }
        let lines: Vec<&str> = output.split('\n').collect();
        let shown = if self.expanded { lines.len() } else { FALLBACK_PREVIEW_LINES.min(lines.len()) };
        let theme = theme();
        let mut content =
            lines[..shown].iter().map(|line| theme.fg(ThemeColor::ToolOutput, line)).collect::<Vec<_>>().join("\n");
        if lines.len() > shown {
            content.push_str(&more_lines_hint(lines.len() - shown, ""));
        }
        Some(text(&content))
    }

    fn format_unknown(&self) -> String {
        let theme = theme();
        let mut content = theme.fg(ThemeColor::ToolTitle, &theme.bold(&self.tool_name));
        if let Ok(args) = serde_json::to_string_pretty(&self.args) {
            content.push_str(&format!("\n\n{args}"));
        }
        if let Some(result) = &self.result {
            let output = get_text_output(result, self.options.show_images);
            if !output.is_empty() {
                content.push_str(&format!("\n{output}"));
            }
        }
        content
    }

    fn context<'a>(
        &'a self,
        state: &'a mut RendererState,
        args: &'a Value,
        cwd: &'a std::path::Path,
    ) -> ToolRenderContext<'a> {
        let dirty = self.self_dirty.clone();
        let tui = self.tui.downgrade();
        ToolRenderContext {
            args,
            tool_call_id: &self.tool_call_id,
            invalidate: Rc::new(move || {
                if let Some(dirty) = dirty.upgrade() {
                    dirty.set(true);
                }
                if let Some(tui) = tui.upgrade() {
                    tui.request_render(false);
                }
            }),
            state,
            cwd,
            execution_started: self.execution_started,
            args_complete: self.args_complete,
            is_partial: self.is_partial,
            expanded: self.expanded,
            show_images: self.options.show_images,
            is_error: self.result.as_ref().is_some_and(|result| result.is_error),
            duration_ms: if self.is_partial { None } else { self.result.as_ref().and_then(|r| r.duration_ms) },
            output_pad: self.options.output_pad,
        }
    }

    fn update_display(&mut self) {
        let background = match &self.result {
            _ if self.is_partial => bg(ThemeBg::ToolPendingBg),
            Some(result) if result.is_error => bg(ThemeBg::ToolErrorBg),
            _ => bg(ThemeBg::ToolSuccessBg),
        };
        match self.display.clone() {
            ToolDisplay::Unknown => {
                let mut text = self.content_text.borrow_mut();
                text.set_custom_bg_fn(Some(background));
                text.set_padding_x(self.options.output_pad);
                text.set_text(&self.format_unknown());
            }
            ToolDisplay::Generic => {
                let call = self.region(text(&format_tool_call_with_args(&self.tool_name, &self.args, self.expanded)));
                let result = self.result_fallback().map(|result| self.region(result));
                self.fill_box(background, call, result);
            }
            ToolDisplay::Renderer(renderer) => {
                let (args, cwd) = (self.args.clone(), self.cwd.clone());
                let mut state = std::mem::take(&mut self.state);
                let mut call = renderer.render_call(&mut self.context(&mut state, &args, &cwd));
                let result = self
                    .result
                    .clone()
                    .and_then(|result| renderer.render_result(&result, &mut self.context(&mut state, &args, &cwd)));
                if self.result.is_some() && renderer.refresh_call_after_result() {
                    call = renderer.render_call(&mut self.context(&mut state, &args, &cwd));
                }
                self.state = state;
                let call = self.region(call);
                let result = result.map(|result| self.region(result));
                self.fill_box(background, call, result);
            }
        }
        self.update_images();
    }

    fn fill_box(&self, background: StyleFn, call: ComponentRef, result: Option<ComponentRef>) {
        let mut content = self.content_box.borrow_mut();
        content.set_bg_fn(Some(background));
        content.set_padding_x(self.options.output_pad);
        content.clear();
        content.add_child(call);
        if let Some(result) = result {
            content.add_child(result);
        }
    }

    fn update_images(&mut self) {
        let previous = std::mem::take(&mut self.images);
        let Some(result) = &self.result else { return };
        if get_capabilities().images.is_none() || !self.options.show_images {
            return;
        }
        let width_cells = self.options.image_width_cells;
        for block in &result.content {
            let ContentBlock::Image { data, mime_type } = block else { continue };
            let index = self.images.len();
            let reused = previous
                .get(index)
                .filter(|old| old.data == *data && old.mime_type == *mime_type && old.width_cells == width_cells)
                .map(|old| old.component.clone());
            let image = reused.unwrap_or_else(|| {
                let theme =
                    ImageTheme { fallback_color: Rc::new(|text: &str| theme().fg(ThemeColor::ToolOutput, text)) };
                let options = ImageOptions { max_width_cells: Some(width_cells), ..ImageOptions::default() };
                component(Image::new(data, mime_type, theme, options, None))
            });
            self.images.push(ImageEntry {
                data: data.clone(),
                mime_type: mime_type.clone(),
                width_cells,
                component: image,
            });
        }
    }
}

impl Component for ToolExecutionComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        if self.dirty.take() {
            self.update_display();
        }
        let mut lines = Spacer::new(1).render(width);
        let content: ComponentRef = match self.display {
            ToolDisplay::Unknown => self.content_text.clone(),
            _ => self.content_box.clone(),
        };
        lines.extend(content.borrow_mut().render(width));
        for image in &self.images {
            lines.push(String::new());
            lines.extend(image.component.borrow_mut().render(width));
        }
        lines
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let mut container = Container::new();
        container.add_child(pi_tui::component(Spacer::new(1)));
        match self.display {
            ToolDisplay::Unknown => container.add_child(self.content_text.clone()),
            _ => container.add_child(self.content_box.clone()),
        }
        let response = container.handle_mouse(event);
        if self.clicked.take() {
            self.set_expanded(!self.expanded);
        }
        response
    }

    fn invalidate(&mut self) {
        self.content_box.borrow_mut().invalidate();
        self.content_text.borrow_mut().invalidate();
        self.update_display();
    }
}
