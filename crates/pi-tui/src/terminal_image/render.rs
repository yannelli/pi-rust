//! Image cell sizing and protocol selection for rendering.

use super::capabilities::{get_capabilities, get_cell_dimensions};
use super::iterm2::{Iterm2EncodeOptions, encode_iterm2};
use super::kitty::{KittyEncodeOptions, KittyImageMetadata, encode_kitty, register_kitty_image_metadata};
use super::{CellDimensions, ImageCellSize, ImageDimensions, ImageProtocol, ImageRenderOptions};

/// A rendered inline image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedImage {
    /// The escape sequence that draws the image.
    pub sequence: String,
    /// Columns covered.
    pub columns: usize,
    /// Rows covered.
    pub rows: usize,
    /// The Kitty image ID, when one was given.
    pub image_id: Option<u32>,
}

fn choose_less_distorted_cell_count(upper_count: usize, ideal_count: f64) -> usize {
    if upper_count <= 1 {
        return upper_count;
    }
    let lower_count = upper_count - 1;
    let distortion = |count: usize| (count as f64 / ideal_count).max(ideal_count / count as f64);
    if distortion(lower_count) < distortion(upper_count) { lower_count } else { upper_count }
}

/// The cells an image covers when scaled to fit `max_width_cells` and, if given, `max_height_cells`.
/// With `optimize_aspect_ratio`, the derived dimension is rounded down when that distorts the image less.
pub fn calculate_image_cell_size(
    image_dimensions: ImageDimensions,
    max_width_cells: usize,
    max_height_cells: Option<usize>,
    cell_dimensions: CellDimensions,
    optimize_aspect_ratio: bool,
) -> ImageCellSize {
    let max_width = max_width_cells.max(1);
    let max_height = max_height_cells.map(|cells| cells.max(1));
    let image_width = f64::from(image_dimensions.width_px.max(1));
    let image_height = f64::from(image_dimensions.height_px.max(1));
    let cell_width = f64::from(cell_dimensions.width_px);
    let cell_height = f64::from(cell_dimensions.height_px);

    let width_scale = (max_width as f64 * cell_width) / image_width;
    let height_scale = max_height.map_or(width_scale, |height| (height as f64 * cell_height) / image_height);
    let scale = width_scale.min(height_scale);

    let scaled_width_px = image_width * scale;
    let scaled_height_px = image_height * scale;
    let mut columns = ((scaled_width_px / cell_width).ceil() as usize).min(max_width).max(1);
    let mut rows = ((scaled_height_px / cell_height).ceil() as usize).max(1);
    if let Some(height) = max_height {
        rows = rows.min(height);
    }

    if !optimize_aspect_ratio {
        return ImageCellSize { columns, rows };
    }

    if width_scale <= height_scale {
        let ideal_rows = (columns as f64 * cell_width * image_height) / (image_width * cell_height);
        rows = choose_less_distorted_cell_count(rows, ideal_rows);
    } else {
        let ideal_columns = (rows as f64 * cell_height * image_width) / (image_height * cell_width);
        columns = choose_less_distorted_cell_count(columns, ideal_columns);
    }
    ImageCellSize { columns, rows }
}

/// The rows an image covers at `target_width_cells` columns.
pub fn calculate_image_rows(
    image_dimensions: ImageDimensions,
    target_width_cells: usize,
    cell_dimensions: CellDimensions,
) -> usize {
    calculate_image_cell_size(image_dimensions, target_width_cells, None, cell_dimensions, false).rows
}

/// Renders base64 image data with the terminal's image protocol, or `None` when images are off.
/// Kitty images with an ID register their metadata for later cropping and re-placement.
pub fn render_image(
    base64_data: &str,
    image_dimensions: ImageDimensions,
    options: &ImageRenderOptions,
) -> Option<RenderedImage> {
    let protocol = get_capabilities().images?;
    let max_width = options.max_width_cells.unwrap_or(80);
    // Reduce Kitty's cell-aligned distortion without shrinking iTerm2 reservations.
    let size = calculate_image_cell_size(
        image_dimensions,
        max_width,
        options.max_height_cells,
        get_cell_dimensions(),
        protocol == ImageProtocol::Kitty,
    );

    match protocol {
        ImageProtocol::Kitty => {
            if let Some(image_id) = options.image_id {
                register_kitty_image_metadata(KittyImageMetadata {
                    image_id,
                    columns: size.columns,
                    rows: size.rows,
                    width_px: image_dimensions.width_px,
                    height_px: image_dimensions.height_px,
                });
            }
            let sequence = encode_kitty(
                base64_data,
                &KittyEncodeOptions {
                    columns: Some(size.columns),
                    rows: Some(size.rows),
                    image_id: options.image_id,
                    move_cursor: options.move_cursor,
                },
            );
            Some(RenderedImage { sequence, columns: size.columns, rows: size.rows, image_id: options.image_id })
        }
        ImageProtocol::Iterm2 => {
            let sequence = encode_iterm2(
                base64_data,
                &Iterm2EncodeOptions {
                    width: Some(size.columns.to_string()),
                    height: Some("auto".to_owned()),
                    preserve_aspect_ratio: Some(options.preserve_aspect_ratio.unwrap_or(true)),
                    ..Iterm2EncodeOptions::default()
                },
            );
            Some(RenderedImage { sequence, columns: size.columns, rows: size.rows, image_id: None })
        }
    }
}
