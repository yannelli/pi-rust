//! Inline images (Kitty graphics protocol and iTerm2 OSC 1337), terminal capability detection,
//! image dimension sniffing, and the OSC 8 hyperlink helper.

mod capabilities;
mod dimensions;
mod fallback;
mod iterm2;
mod kitty;
mod render;

pub use capabilities::{
    CapabilityOverrides, detect_capabilities, detect_capabilities_with, get_capabilities, get_capabilities_with,
    get_cell_dimensions, get_terminal_color_mode, probe_tmux_hyperlinks, reset_capabilities_cache, set_capabilities,
    set_capability_overrides, set_cell_dimensions,
};
pub use dimensions::{
    get_gif_dimensions, get_image_dimensions, get_jpeg_dimensions, get_png_dimensions, get_webp_dimensions,
};
pub use fallback::{hyperlink, image_fallback, image_fallback_with_home, path_to_file_url};
pub use iterm2::{Iterm2EncodeOptions, encode_iterm2};
pub use kitty::{
    KittyEncodeOptions, KittyImageMetadata, KittyImagePlacement, allocate_image_id, crop_kitty_image_line,
    delete_all_kitty_images, delete_all_kitty_placements, delete_kitty_image, encode_kitty, get_kitty_image_metadata,
    get_kitty_image_placement, get_kitty_image_placement_rows, is_image_line, register_kitty_image_metadata,
};
pub use render::{RenderedImage, calculate_image_cell_size, calculate_image_rows, render_image};

/// An inline image protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageProtocol {
    /// The Kitty graphics protocol (APC `_G`).
    Kitty,
    /// iTerm2 inline images (OSC 1337).
    Iterm2,
}

/// What the terminal supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TerminalCapabilities {
    /// The inline image protocol, or `None` when images are not shown.
    pub images: Option<ImageProtocol>,
    /// Whether 24-bit color is supported.
    pub true_color: bool,
    /// Whether OSC 8 hyperlinks are shown as links.
    pub hyperlinks: bool,
}

/// The pixel size of one terminal cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellDimensions {
    /// Cell width in pixels.
    pub width_px: u32,
    /// Cell height in pixels.
    pub height_px: u32,
}

impl Default for CellDimensions {
    fn default() -> Self {
        Self { width_px: 9, height_px: 18 }
    }
}

/// The pixel size of an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageDimensions {
    /// Image width in pixels.
    pub width_px: u32,
    /// Image height in pixels.
    pub height_px: u32,
}

/// How many cells an image covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageCellSize {
    /// Columns covered.
    pub columns: usize,
    /// Rows covered.
    pub rows: usize,
}

/// Options for [`render_image`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImageRenderOptions {
    /// Widest the image may be, in cells. Defaults to 80.
    pub max_width_cells: Option<usize>,
    /// Tallest the image may be, in cells.
    pub max_height_cells: Option<usize>,
    /// iTerm2 only: keep the aspect ratio. Defaults to true.
    pub preserve_aspect_ratio: Option<bool>,
    /// Kitty image ID. When set, reuses or replaces the image with this ID.
    pub image_id: Option<u32>,
    /// Whether Kitty applies its default cursor movement after placement. Defaults to true.
    pub move_cursor: Option<bool>,
}
