//! Test-only ports of upstream components that live in the components stream (`Text`,
//! `MouseRegion`, `Image`), so renderer tests can follow the upstream cases.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::terminal_image::{
    ImageDimensions, ImageProtocol, ImageRenderOptions, allocate_image_id, get_capabilities, get_cell_dimensions,
    get_image_dimensions, image_fallback, render_image,
};
use pi_tui::utils::{apply_background_to_line, flatten_lines, truncate_to_width, visible_width, wrap_text_with_ansi};
use pi_tui::{
    Component, ComponentRef, MouseResponse, TuiMouseEvent, TuiMouseEventResult, component, dispatch_mouse_event,
};

pub type BgFn = Box<dyn Fn(&str) -> String>;

/// Port of upstream `Text`: wrapped text with horizontal and vertical padding.
pub struct Text {
    text: String,
    padding_x: usize,
    padding_y: usize,
    bg: Option<BgFn>,
    cache: Option<(usize, Vec<String>)>,
}

impl Text {
    pub fn new(text: &str, padding_x: usize, padding_y: usize) -> Self {
        Self { text: text.to_string(), padding_x, padding_y, bg: None, cache: None }
    }

    /// Upstream `new Text(text)` defaults: one column and one row of padding.
    pub fn plain(text: &str) -> Self {
        Self::new(text, 1, 1)
    }

    pub fn with_bg(mut self, bg: impl Fn(&str) -> String + 'static) -> Self {
        self.bg = Some(Box::new(bg));
        self
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cache = None;
    }

    pub fn set_padding_x(&mut self, padding_x: usize) {
        self.padding_x = padding_x;
        self.cache = None;
    }
}

impl Component for Text {
    fn render(&mut self, width: usize) -> Vec<String> {
        if let Some((cached_width, lines)) = &self.cache
            && *cached_width == width
        {
            return lines.clone();
        }
        if self.text.trim().is_empty() {
            self.cache = Some((width, Vec::new()));
            return Vec::new();
        }
        let normalized = self.text.replace('\t', "   ");
        let padding_x = self.padding_x.min(width.saturating_sub(1) / 2);
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let margin = " ".repeat(padding_x);
        let mut content = Vec::new();
        for line in wrap_text_with_ansi(&normalized, content_width) {
            let with_margins = format!("{margin}{line}{margin}");
            match &self.bg {
                Some(bg) => content.push(apply_background_to_line(&with_margins, width, bg.as_ref())),
                None => {
                    let pad = width.saturating_sub(visible_width(&with_margins));
                    content.push(format!("{with_margins}{}", " ".repeat(pad)));
                }
            }
        }
        let empty = " ".repeat(width);
        let empty_line = match &self.bg {
            Some(bg) => apply_background_to_line(&empty, width, bg.as_ref()),
            None => empty,
        };
        let mut result = vec![empty_line.clone(); self.padding_y];
        result.extend(content);
        result.extend(vec![empty_line; self.padding_y]);
        flatten_lines(&mut result);
        self.cache = Some((width, result.clone()));
        if result.is_empty() { vec![String::new()] } else { result }
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }
}

pub type MouseRegionHandler = Box<dyn FnMut(&TuiMouseEvent) -> Option<TuiMouseEventResult>>;

/// Port of upstream `MouseRegion`: adds a mouse handler around a child without changing rendering.
pub struct MouseRegion {
    child: ComponentRef,
    on_mouse: MouseRegionHandler,
}

impl MouseRegion {
    pub fn new(
        child: ComponentRef,
        on_mouse: impl FnMut(&TuiMouseEvent) -> Option<TuiMouseEventResult> + 'static,
    ) -> Self {
        Self { child, on_mouse: Box::new(on_mouse) }
    }
}

impl Component for MouseRegion {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.child.borrow_mut().render(width)
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        if let Some(result) = dispatch_mouse_event(&self.child, event) {
            return Some(MouseResponse::Forwarded(result));
        }
        (self.on_mouse)(event).map(MouseResponse::Handled)
    }

    fn invalidate(&mut self) {
        self.child.borrow_mut().invalidate();
    }
}

/// Options of the test [`Image`].
#[derive(Default, Clone)]
pub struct ImageOptions {
    pub max_width_cells: Option<usize>,
    pub max_height_cells: Option<usize>,
    pub filename: Option<String>,
    pub image_id: Option<u32>,
}

/// Port of upstream `Image` for PNG data (no transcoder).
pub struct Image {
    data: String,
    mime_type: String,
    dimensions: ImageDimensions,
    options: ImageOptions,
    image_id: Option<u32>,
    cache: Option<(usize, Vec<String>)>,
}

impl Image {
    pub fn new(data: &str, mime_type: &str, options: ImageOptions, dimensions: Option<ImageDimensions>) -> Self {
        let dimensions = dimensions
            .or_else(|| get_image_dimensions(data, mime_type))
            .unwrap_or(ImageDimensions { width_px: 800, height_px: 600 });
        let image_id = options.image_id;
        Self { data: data.to_string(), mime_type: mime_type.to_string(), dimensions, options, image_id, cache: None }
    }

    pub fn image_id(&self) -> Option<u32> {
        self.image_id
    }

    fn fallback(&self, width: usize) -> Vec<String> {
        let text = image_fallback(&self.mime_type, Some(self.dimensions), self.options.filename.as_deref());
        vec![truncate_to_width(&text, width)]
    }
}

impl Component for Image {
    fn render(&mut self, width: usize) -> Vec<String> {
        if let Some((cached_width, lines)) = &self.cache
            && *cached_width == width
        {
            return lines.clone();
        }
        let max_width = width.saturating_sub(2).min(self.options.max_width_cells.unwrap_or(60)).max(1);
        let cell = get_cell_dimensions();
        let default_max_height =
            ((max_width as f64 * f64::from(cell.width_px)) / f64::from(cell.height_px)).ceil().max(1.0) as usize;
        let max_height = self.options.max_height_cells.unwrap_or(default_max_height);
        let caps = get_capabilities();
        let usable =
            caps.images.is_some() && (caps.images != Some(ImageProtocol::Kitty) || self.mime_type == "image/png");
        if usable && caps.images == Some(ImageProtocol::Kitty) && self.image_id.is_none() {
            self.image_id = Some(allocate_image_id());
        }
        let options = ImageRenderOptions {
            max_width_cells: Some(max_width),
            max_height_cells: Some(max_height),
            image_id: self.image_id,
            move_cursor: Some(false),
            ..ImageRenderOptions::default()
        };
        let rendered = if usable { render_image(&self.data, self.dimensions, &options) } else { None };
        let lines = match rendered {
            Some(result) => {
                if result.image_id.is_some() {
                    self.image_id = result.image_id;
                }
                if caps.images == Some(ImageProtocol::Kitty) {
                    let mut lines = vec![result.sequence];
                    lines.extend(std::iter::repeat_n(String::new(), result.rows.saturating_sub(1)));
                    lines
                } else {
                    let offset = result.rows.saturating_sub(1);
                    let mut lines = vec![String::new(); offset];
                    let move_up = if offset > 0 { format!("\x1b[{offset}A") } else { String::new() };
                    lines.push(format!("{move_up}{}", result.sequence));
                    lines
                }
            }
            None => self.fallback(width),
        };
        self.cache = Some((width, lines.clone()));
        lines
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }
}

/// Wraps a component as a shared handle typed as [`ComponentRef`].
pub fn shared<C: Component + 'static>(value: C) -> (Rc<RefCell<C>>, ComponentRef) {
    let typed = component(value);
    let erased: ComponentRef = typed.clone();
    (typed, erased)
}
