//! Port of the encoding, Kitty placement, cell sizing, fallback, and hyperlink cases in upstream
//! `test/terminal-image.test.ts`. Cases built on the `Image` component call `render_image` with the
//! options that component derives; its line padding is tested with that component.

use std::path::PathBuf;
use std::sync::MutexGuard;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use pi_tui::env::global_state_lock;
use pi_tui::terminal_image::{
    CellDimensions, ImageDimensions, ImageProtocol, ImageRenderOptions, Iterm2EncodeOptions, KittyEncodeOptions,
    KittyImageMetadata, RenderedImage, TerminalCapabilities, allocate_image_id, crop_kitty_image_line,
    delete_all_kitty_images, delete_all_kitty_placements, delete_kitty_image, encode_iterm2, encode_kitty,
    get_kitty_image_metadata, get_kitty_image_placement, get_kitty_image_placement_rows, get_png_dimensions, hyperlink,
    image_fallback, register_kitty_image_metadata, render_image, reset_capabilities_cache, set_capabilities,
    set_cell_dimensions,
};
use regex::Regex;

/// Holds the global state lock and restores capabilities and cell dimensions on drop.
struct StateGuard {
    _lock: MutexGuard<'static, ()>,
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        reset_capabilities_cache();
        set_cell_dimensions(CellDimensions { width_px: 9, height_px: 18 });
    }
}

fn setup(images: Option<ImageProtocol>, hyperlinks: bool, cell: (u32, u32)) -> StateGuard {
    let lock = global_state_lock();
    set_capabilities(TerminalCapabilities { images, true_color: images.is_some(), hyperlinks });
    set_cell_dimensions(CellDimensions { width_px: cell.0, height_px: cell.1 });
    StateGuard { _lock: lock }
}

fn dims(width_px: u32, height_px: u32) -> ImageDimensions {
    ImageDimensions { width_px, height_px }
}

fn render(width_px: u32, height_px: u32, options: ImageRenderOptions) -> RenderedImage {
    render_image("AAAA", dims(width_px, height_px), &options).expect("images are enabled")
}

/// The options the `Image` component passes for a render width and its `maxWidthCells` option.
fn component_options(
    width: usize,
    max_width_cells: usize,
    cell: (u32, u32),
    image_id: Option<u32>,
) -> ImageRenderOptions {
    let max_width = max_width_cells.min(width - 2).max(1);
    let max_height = ((max_width as u32 * cell.0).div_ceil(cell.1) as usize).max(1);
    ImageRenderOptions {
        max_width_cells: Some(max_width),
        max_height_cells: Some(max_height),
        image_id,
        move_cursor: Some(false),
        ..ImageRenderOptions::default()
    }
}

#[test]
fn includes_the_decoded_payload_size_in_osc_1337_metadata() {
    let options =
        Iterm2EncodeOptions { width: Some("2".to_owned()), height: Some("auto".to_owned()), ..Default::default() };
    assert_eq!(encode_iterm2("AAAA", &options), "\x1b]1337;File=inline=1;size=3;width=2;height=auto:AAAA\x07");
    let named = Iterm2EncodeOptions {
        name: Some("shot.png".to_owned()),
        preserve_aspect_ratio: Some(false),
        inline: Some(false),
        ..Default::default()
    };
    let expected = format!(
        "\x1b]1337;File=inline=0;size=4;name={};preserveAspectRatio=0:AAAAAA==\x07",
        STANDARD.encode("shot.png")
    );
    assert_eq!(encode_iterm2("AAAAAA==", &named), expected);
}

#[test]
fn can_request_no_terminal_side_cursor_movement() {
    let options =
        KittyEncodeOptions { columns: Some(2), rows: Some(2), move_cursor: Some(false), ..Default::default() };
    assert!(encode_kitty("AAAA", &options).starts_with("\x1b_Ga=T,f=100,q=2,C=1,c=2,r=2;"));
}

#[test]
fn reads_explicit_placement_rows_without_registered_metadata() {
    let options =
        KittyEncodeOptions { columns: Some(2), rows: Some(3), move_cursor: Some(false), ..Default::default() };
    assert_eq!(get_kitty_image_placement_rows(&encode_kitty("AAAA", &options)), Some(3));
}

#[test]
fn suppresses_kitty_replies_for_delete_commands() {
    assert_eq!(delete_kitty_image(42), "\x1b_Ga=d,d=I,i=42,q=2\x1b\\");
    assert_eq!(delete_all_kitty_images(), "\x1b_Ga=d,d=A,q=2\x1b\\");
    assert_eq!(delete_all_kitty_placements(), "\x1b_Ga=d,d=a,q=2\x1b\\");
}

#[test]
fn chunks_large_kitty_payloads() {
    let options = KittyEncodeOptions { image_id: Some(7), ..Default::default() };
    let data = "B".repeat(4096 * 2 + 10);
    let sequence = encode_kitty(&data, &options);
    let expected = format!(
        "\x1b_Ga=T,f=100,q=2,i=7,m=1;{}\x1b\\\x1b_Gm=1;{}\x1b\\\x1b_Gm=0;{}\x1b\\",
        "B".repeat(4096),
        "B".repeat(4096),
        "B".repeat(10)
    );
    assert_eq!(sequence, expected);
}

#[test]
fn preserves_render_images_default_terminal_side_cursor_movement() {
    let _state = setup(Some(ImageProtocol::Kitty), true, (10, 10));
    let result = render(20, 20, ImageRenderOptions { max_width_cells: Some(2), ..Default::default() });
    assert!(!result.sequence.contains(",C=1,"));
    assert_eq!(result.rows, 2);
}

#[test]
fn can_opt_render_image_into_no_terminal_side_cursor_movement() {
    let _state = setup(Some(ImageProtocol::Kitty), true, (10, 10));
    let options = ImageRenderOptions { max_width_cells: Some(2), move_cursor: Some(false), ..Default::default() };
    let result = render(20, 20, options);
    assert!(result.sequence.contains(",C=1,"));
    assert_eq!(result.rows, 2);
}

#[test]
fn registers_metadata_and_crops_a_partially_visible_placement() {
    let _state = setup(Some(ImageProtocol::Kitty), true, (10, 10));
    let options = ImageRenderOptions {
        max_width_cells: Some(3),
        image_id: Some(42),
        move_cursor: Some(false),
        ..Default::default()
    };
    let result = render(100, 100, options);
    let metadata = KittyImageMetadata { image_id: 42, columns: 3, rows: 3, width_px: 100, height_px: 100 };
    assert_eq!(get_kitty_image_metadata(&result.sequence), Some(metadata));
    assert!(crop_kitty_image_line(&result.sequence, 2, 1).contains("y=66,h=34,r=1"));
    assert_eq!(crop_kitty_image_line(&result.sequence, 0, 3), result.sequence);
    assert_eq!(crop_kitty_image_line(&result.sequence, 3, 1), result.sequence);
}

#[test]
fn creates_placement_only_commands_for_uploaded_and_cropped_images() {
    let _state = setup(None, false, (9, 18));
    register_kitty_image_metadata(KittyImageMetadata {
        image_id: 42,
        columns: 3,
        rows: 3,
        width_px: 100,
        height_px: 100,
    });
    let options = KittyEncodeOptions { columns: Some(3), rows: Some(3), image_id: Some(42), move_cursor: Some(false) };
    let transmission = encode_kitty(&"A".repeat(8192), &options);
    let line = format!("left {} right", crop_kitty_image_line(&transmission, 2, 1));
    let placement = get_kitty_image_placement(&line).expect("placement");
    assert_eq!(get_kitty_image_placement_rows(&line), Some(1));
    assert_eq!(placement.transmission_bytes, line.len() - "left ".len() - " right".len());
    assert_eq!(placement.estimated_decoded_bytes, 100 * 100 * 4);
    assert_eq!(placement.rows, 1);
    assert_eq!(placement.sequence, "\x1b_Ga=p,q=2,C=1,c=3,i=42,y=66,h=34,r=1\x1b\\");
    assert_eq!(placement.replacement_line, format!("left {} right", placement.sequence));
    assert!(!placement.replacement_line.contains("AAAA"));
}

#[test]
fn honors_max_height_cells_by_reducing_rendered_width() {
    let _state = setup(Some(ImageProtocol::Kitty), true, (10, 10));
    let result = render(
        10,
        100,
        ImageRenderOptions { max_width_cells: Some(10), max_height_cells: Some(5), ..Default::default() },
    );
    assert_eq!(result.rows, 5);
    assert!(result.sequence.contains(",c=1,r=5"));
}

#[test]
fn caps_component_height_to_a_square_pixel_box_by_default() {
    let _state = setup(Some(ImageProtocol::Kitty), true, (10, 20));
    let result = render(10, 100, component_options(12, 10, (10, 20), Some(allocate_image_id())));
    assert_eq!(result.rows, 5);
    assert!(result.sequence.contains(",c=1,r=5"));
}

#[test]
fn places_the_image_sequence_with_its_id() {
    let _state = setup(Some(ImageProtocol::Kitty), true, (10, 10));
    let image_id = allocate_image_id();
    let result = render(20, 20, component_options(4, 2, (10, 10), Some(image_id)));
    assert!(result.sequence.starts_with("\x1b_G"));
    assert!(result.sequence.contains(",C=1,"));
    assert!(result.sequence.contains(&format!(",i={image_id}")));
    assert!(result.sequence.ends_with("\x1b\\"));
    assert_eq!(result.rows, 2);
    assert_eq!(result.image_id, Some(image_id));
}

#[test]
fn allocates_ids_in_the_kitty_range() {
    let ids: Vec<u32> = (0..64).map(|_| allocate_image_id()).collect();
    assert!(ids.iter().all(|&id| id >= 1));
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert!(unique.len() > 60);
}

#[test]
fn reserves_at_least_one_kitty_row_for_thin_images() {
    let _state = setup(Some(ImageProtocol::Kitty), true, (9, 18));
    let result = render(1200, 12, ImageRenderOptions { max_width_cells: Some(60), ..Default::default() });
    assert_eq!(result.rows, 1);
    assert!(result.sequence.contains(",c=60,r=1;"));
}

#[test]
fn keeps_kitty_placement_and_crop_metadata_consistent_across_width_changes() {
    let _state = setup(Some(ImageProtocol::Kitty), true, (9, 18));
    let result = render(615, 86, component_options(62, 60, (9, 18), Some(8938)));
    assert_eq!(result.rows, 4);
    assert!(result.sequence.contains(",c=60,r=4,i=8938;"));
    let metadata = KittyImageMetadata { image_id: 8938, columns: 60, rows: 4, width_px: 615, height_px: 86 };
    assert_eq!(get_kitty_image_metadata(&result.sequence), Some(metadata));
    let cropped = crop_kitty_image_line(&result.sequence, 1, 2);
    assert_eq!(
        get_kitty_image_placement(&cropped).map(|placement| placement.sequence).as_deref(),
        Some("\x1b_Ga=p,q=2,C=1,c=60,i=8938,y=21,h=44,r=2\x1b\\")
    );

    let narrower = render(615, 86, component_options(32, 60, (9, 18), Some(8938)));
    assert_eq!(narrower.rows, 2);
    assert!(narrower.sequence.contains(",c=30,r=2,i=8938;"));
    assert_eq!(get_kitty_image_metadata(&narrower.sequence).map(|metadata| metadata.rows), Some(2));
}

#[test]
fn keeps_the_ceiling_placement_when_rounding_down_would_increase_distortion() {
    let _state = setup(Some(ImageProtocol::Kitty), true, (15, 28));
    let result = render(615, 86, ImageRenderOptions { max_width_cells: Some(60), ..Default::default() });
    assert_eq!(result.rows, 5);
    assert!(result.sequence.contains(",c=60,r=5;"));
}

#[test]
fn keeps_height_limited_kitty_columns_and_crop_metadata_consistent() {
    let _state = setup(Some(ImageProtocol::Kitty), true, (14, 28));
    let result = render(400, 900, component_options(32, 30, (14, 28), Some(8938)));
    assert_eq!(result.rows, 15);
    assert!(result.sequence.contains(",c=13,r=15,i=8938;"));
    let metadata = KittyImageMetadata { image_id: 8938, columns: 13, rows: 15, width_px: 400, height_px: 900 };
    assert_eq!(get_kitty_image_metadata(&result.sequence), Some(metadata));
    assert_eq!(
        get_kitty_image_placement(&crop_kitty_image_line(&result.sequence, 1, 2)).map(|placement| placement.sequence),
        Some("\x1b_Ga=p,q=2,C=1,c=13,i=8938,y=60,h=120,r=2\x1b\\".to_owned())
    );
    let narrower = render(400, 900, component_options(22, 30, (14, 28), Some(8938)));
    assert_eq!(narrower.rows, 10);
    assert!(narrower.sequence.contains(",c=9,r=10,i=8938;"));
}

#[test]
fn chooses_thin_kitty_widths_by_proportions_while_keeping_at_least_one_column() {
    let _state = setup(Some(ImageProtocol::Kitty), true, (1, 1));
    for (width_px, columns) in [(1, 1), (140, 1), (149, 2)] {
        let options =
            ImageRenderOptions { max_width_cells: Some(30), max_height_cells: Some(10), ..Default::default() };
        let result = render(width_px, 1000, options);
        assert_eq!(result.columns, columns);
        assert_eq!(result.rows, 10);
        assert!(result.sequence.contains(&format!(",c={columns},r=10;")));
    }
}

#[test]
fn keeps_iterm2_ceiling_width_when_height_limited() {
    let _state = setup(Some(ImageProtocol::Iterm2), true, (14, 28));
    let options = ImageRenderOptions { max_width_cells: Some(30), max_height_cells: Some(15), ..Default::default() };
    let result = render(400, 900, options);
    assert_eq!(result.columns, 14);
    assert_eq!(result.rows, 15);
    assert_eq!(result.sequence, "\x1b]1337;File=inline=1;size=3;width=14;height=auto:AAAA\x07");
}

#[test]
fn keeps_iterm2_ceiling_based_reserved_rows() {
    let _state = setup(Some(ImageProtocol::Iterm2), true, (9, 18));
    let result = render(615, 86, component_options(62, 60, (9, 18), None));
    assert_eq!(result.rows, 5);
    assert_eq!(result.sequence, "\x1b]1337;File=inline=1;size=3;width=60;height=auto:AAAA\x07");
}

#[test]
fn renders_nothing_without_an_image_protocol() {
    let _state = setup(None, false, (9, 18));
    assert_eq!(render_image("AAAA", dims(10, 10), &ImageRenderOptions::default()), None);
}

fn home_path(parts: &[&str]) -> PathBuf {
    let mut path = std::env::home_dir().expect("home directory");
    path.extend(parts);
    path
}

/// `~/.pi/agent/shot.png` with native separators, as the fallback shows it.
fn short_shot() -> String {
    ["~", ".pi", "agent", "shot.png"].join(std::path::MAIN_SEPARATOR_STR)
}

#[test]
fn shortens_home_prefixed_absolute_paths_without_hyperlinks() {
    let _state = setup(None, false, (9, 18));
    let abs = home_path(&[".pi", "agent", "shot.png"]);
    let result = image_fallback("image/png", Some(dims(1280, 720)), Some(&abs.to_string_lossy()));
    assert_eq!(result, format!("[Image: {} [image/png] 1280x720]", short_shot()));
}

#[test]
fn wraps_shortened_absolute_paths_in_osc_8_file_links_when_hyperlinks_are_enabled() {
    let _state = setup(None, true, (9, 18));
    let abs = home_path(&[".pi", "agent", "shot.png"]).to_string_lossy().into_owned();
    let result = image_fallback("image/png", Some(dims(10, 10)), Some(&abs));
    assert!(result.contains("\x1b]8;;file://"), "expected OSC 8 file link");
    assert!(result.contains(&abs.replace('\\', "/")) || result.contains(&abs), "file URL should target the path");
    let visible = Regex::new(r"\x1b\]8;;.*?\x1b\\").unwrap().replace_all(&result, "");
    assert_eq!(visible, format!("[Image: {} [image/png] 10x10]", short_shot()));
}

#[test]
fn leaves_bare_basenames_unchanged_and_does_not_hyperlink_them() {
    let _state = setup(None, true, (9, 18));
    let result = image_fallback("image/png", Some(dims(1, 1)), Some("clankolas.png"));
    assert_eq!(result, "[Image: clankolas.png [image/png] 1x1]");
    assert!(!result.contains("\x1b]8;"));
}

#[test]
fn omits_filename_segment_when_not_provided() {
    let _state = setup(None, false, (9, 18));
    assert_eq!(image_fallback("image/png", Some(dims(8, 6)), None), "[Image: [image/png] 8x6]");
}

#[cfg(unix)]
#[test]
fn percent_encodes_file_links_like_node() {
    let _state = setup(None, true, (9, 18));
    let result = image_fallback("image/png", None, Some("/tmp/a b/./c#?%é.png"));
    assert!(result.contains("\x1b]8;;file:///tmp/a%20b/c%23%3F%25%C3%A9.png\x1b\\"), "{result:?}");
}

#[test]
fn reads_png_dimensions_from_a_minimal_header() {
    let png = STANDARD.encode([
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x28, 0x00, 0x00, 0x00, 0x0a,
    ]);
    assert_eq!(get_png_dimensions(&png), Some(dims(40, 10)));
}

#[test]
fn wraps_text_in_osc_8_open_and_close_sequences() {
    assert_eq!(hyperlink("click me", "https://example.com"), "\x1b]8;;https://example.com\x1b\\click me\x1b]8;;\x1b\\");
    let styled = "\x1b[4m\x1b[34mclick me\x1b[0m";
    let result = hyperlink(styled, "https://example.com");
    assert!(result.starts_with("\x1b]8;;https://example.com\x1b\\"));
    assert!(result.contains(styled));
    assert!(result.ends_with("\x1b]8;;\x1b\\"));
    assert_eq!(hyperlink("", "https://example.com"), "\x1b]8;;https://example.com\x1b\\\x1b]8;;\x1b\\");
    let file = hyperlink("README.md", "file:///home/user/README.md");
    assert!(file.contains("file:///home/user/README.md"));
    assert!(file.contains("README.md"));
}
