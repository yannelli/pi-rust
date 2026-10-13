//! [`Image`]: an inline image (Kitty or iTerm2), or a text fallback when the terminal cannot show it.

use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};

use crate::component::Component;
use crate::components::scroll_view::StyleFn;
use crate::terminal_image::{
    ImageDimensions, ImageProtocol, ImageRenderOptions, allocate_image_id, get_capabilities, get_cell_dimensions,
    get_image_dimensions, get_png_dimensions, image_fallback, render_image,
};
use crate::utils::truncate_to_width;

/// Converts base64 image data to base64 PNG data, or returns `None` if it cannot. Called
/// synchronously during rendering.
pub type ImageTranscoder = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

const PNG_CACHE_SIZE: usize = 32;

#[derive(Default)]
struct TranscoderState {
    transcoder: Option<ImageTranscoder>,
    /// Least recently used first, keyed by source data.
    png_cache: Vec<(String, Option<String>)>,
}

static TRANSCODER: Mutex<Option<TranscoderState>> = Mutex::new(None);

/// Registers the converter used for non-PNG images on Kitty-protocol terminals, which only accept
/// PNG. Without one, such images render as text fallbacks.
pub fn set_image_transcoder(transcoder: Option<ImageTranscoder>) {
    let mut state = TRANSCODER.lock().unwrap_or_else(PoisonError::into_inner);
    *state = Some(TranscoderState { transcoder, png_cache: Vec::new() });
}

fn to_png(base64_data: &str, mime_type: &str) -> Option<String> {
    let mut guard = TRANSCODER.lock().unwrap_or_else(PoisonError::into_inner);
    let state = guard.as_mut()?;
    let transcoder = state.transcoder.clone()?;
    let png = match state.png_cache.iter().position(|(key, _)| key == base64_data) {
        Some(index) => state.png_cache.remove(index).1,
        None => transcoder(base64_data, mime_type),
    };
    state.png_cache.push((base64_data.to_string(), png.clone()));
    if state.png_cache.len() > PNG_CACHE_SIZE {
        state.png_cache.remove(0);
    }
    png
}

/// Styling of the text fallback.
#[derive(Clone)]
pub struct ImageTheme {
    pub fallback_color: StyleFn,
}

impl Default for ImageTheme {
    /// No styling.
    fn default() -> Self {
        Self { fallback_color: Rc::new(|text| text.to_string()) }
    }
}

/// Options of [`Image::new`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImageOptions {
    /// Widest the image may be, in cells. Defaults to 60.
    pub max_width_cells: Option<usize>,
    /// Tallest the image may be, in cells. Defaults to a square in cells of the width.
    pub max_height_cells: Option<usize>,
    /// Shown in the text fallback.
    pub filename: Option<String>,
    /// Kitty image ID. If provided, reuses this ID (for animations and updates).
    pub image_id: Option<u32>,
}

/// An inline image (upstream `Image`).
pub struct Image {
    base64_data: String,
    mime_type: String,
    dimensions: ImageDimensions,
    theme: ImageTheme,
    options: ImageOptions,
    image_id: Option<u32>,
    /// Converted PNG data for Kitty. Failures are not stored so a later transcoder can retry.
    png_data: Option<String>,
    cache: Option<(usize, Vec<String>)>,
}

impl Image {
    pub fn new(
        base64_data: &str,
        mime_type: &str,
        theme: ImageTheme,
        options: ImageOptions,
        dimensions: Option<ImageDimensions>,
    ) -> Self {
        let dimensions = dimensions
            .or_else(|| get_image_dimensions(base64_data, mime_type))
            .unwrap_or(ImageDimensions { width_px: 800, height_px: 600 });
        let image_id = options.image_id;
        Self {
            base64_data: base64_data.to_string(),
            mime_type: mime_type.to_string(),
            dimensions,
            theme,
            options,
            image_id,
            png_data: None,
            cache: None,
        }
    }

    /// The Kitty image ID used by this image, if any.
    pub fn image_id(&self) -> Option<u32> {
        self.image_id
    }

    fn fallback(&self, width: usize) -> Vec<String> {
        let text = image_fallback(&self.mime_type, Some(self.dimensions), self.options.filename.as_deref());
        vec![truncate_to_width(&(self.theme.fallback_color)(&text), width)]
    }

    fn render_lines(&mut self, width: usize) -> Vec<String> {
        let max_width = width.saturating_sub(2).min(self.options.max_width_cells.unwrap_or(60)).max(1);
        let cell = get_cell_dimensions();
        let default_max_height =
            ((max_width as f64 * f64::from(cell.width_px)) / f64::from(cell.height_px)).ceil().max(1.0) as usize;
        let max_height = self.options.max_height_cells.unwrap_or(default_max_height);

        let caps = get_capabilities();
        let Some(protocol) = caps.images else {
            return self.fallback(width);
        };
        let mut dimensions = self.dimensions;
        let data = if protocol == ImageProtocol::Kitty && self.mime_type != "image/png" {
            if self.png_data.is_none() {
                self.png_data = to_png(&self.base64_data, &self.mime_type);
            }
            let Some(png) = self.png_data.clone() else {
                return self.fallback(width);
            };
            // Conversion may apply EXIF rotation, so prefer the PNG's own dimensions.
            dimensions = get_png_dimensions(&png).unwrap_or(dimensions);
            png
        } else {
            self.base64_data.clone()
        };

        if protocol == ImageProtocol::Kitty && self.image_id.is_none() {
            self.image_id = Some(allocate_image_id());
        }
        let options = ImageRenderOptions {
            max_width_cells: Some(max_width),
            max_height_cells: Some(max_height),
            image_id: self.image_id,
            move_cursor: Some(false),
            ..ImageRenderOptions::default()
        };
        let Some(result) = render_image(&data, dimensions, &options) else {
            return self.fallback(width);
        };
        if result.image_id.is_some() {
            self.image_id = result.image_id;
        }
        let rows = result.rows.max(1);
        if protocol == ImageProtocol::Kitty {
            // C=1 keeps the cursor in place; the extra rows let the TUI account for the height.
            let mut lines = vec![result.sequence];
            lines.extend(std::iter::repeat_n(String::new(), rows - 1));
            lines
        } else {
            // Rows above are cleared first; the last line moves up, draws, and stays in the scroll area.
            let offset = rows - 1;
            let mut lines = vec![String::new(); offset];
            let move_up = if offset > 0 { format!("\x1b[{offset}A") } else { String::new() };
            lines.push(format!("{move_up}{}", result.sequence));
            lines
        }
    }
}

impl Component for Image {
    fn render(&mut self, width: usize) -> Vec<String> {
        if let Some((cached_width, lines)) = &self.cache
            && *cached_width == width
        {
            return lines.clone();
        }
        let lines = self.render_lines(width);
        self.cache = Some((width, lines.clone()));
        lines
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }
}
