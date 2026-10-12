//! Kitty graphics protocol: upload, deletion, image metadata, placements, and cropping.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher as _, Hasher as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;

const KITTY_PREFIX: &str = "\x1b_G";
const ITERM2_PREFIX: &str = "\x1b]1337;File=";
const CHUNK_SIZE: usize = 4096;
const MAX_REGISTERED_IMAGES: usize = 1000;

/// Whether `line` contains a Kitty or iTerm2 image sequence anywhere.
/// Multi-row images can start with a cursor-up prefix, so the whole line is searched.
pub fn is_image_line(line: &str) -> bool {
    line.starts_with(KITTY_PREFIX)
        || line.starts_with(ITERM2_PREFIX)
        || line.contains(KITTY_PREFIX)
        || line.contains(ITERM2_PREFIX)
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);
static ID_HASHER: LazyLock<RandomState> = LazyLock::new(RandomState::new);

/// A random Kitty image ID in `1..=0xffffffff`. Random IDs avoid collisions with
/// images placed by other code that writes to the same terminal.
pub fn allocate_image_id() -> u32 {
    let mut hasher = ID_HASHER.build_hasher();
    hasher.write_u64(ID_COUNTER.fetch_add(1, Ordering::Relaxed));
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_nanos()).unwrap_or(0);
    hasher.write_u128(nanos);
    (hasher.finish() % 0xffff_fffe) as u32 + 1
}

/// Options for [`encode_kitty`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KittyEncodeOptions {
    /// Columns to cover. Zero is omitted.
    pub columns: Option<usize>,
    /// Rows to cover. Zero is omitted.
    pub rows: Option<usize>,
    /// Image ID. Zero is omitted.
    pub image_id: Option<u32>,
    /// Whether Kitty applies its default cursor movement after placement. Defaults to true.
    pub move_cursor: Option<bool>,
}

fn chunk_end(data: &str, start: usize) -> usize {
    let mut end = (start + CHUNK_SIZE).min(data.len());
    while !data.is_char_boundary(end) {
        end += 1;
    }
    end
}

/// Encodes base64 PNG data as a Kitty command that uploads and shows the image (`a=T`),
/// split into 4096-byte chunks.
pub fn encode_kitty(base64_data: &str, options: &KittyEncodeOptions) -> String {
    let mut params = vec!["a=T".to_owned(), "f=100".to_owned(), "q=2".to_owned()];
    if options.move_cursor == Some(false) {
        params.push("C=1".to_owned());
    }
    if let Some(columns) = options.columns.filter(|&value| value != 0) {
        params.push(format!("c={columns}"));
    }
    if let Some(rows) = options.rows.filter(|&value| value != 0) {
        params.push(format!("r={rows}"));
    }
    if let Some(image_id) = options.image_id.filter(|&value| value != 0) {
        params.push(format!("i={image_id}"));
    }
    let params = params.join(",");

    if base64_data.len() <= CHUNK_SIZE {
        return format!("\x1b_G{params};{base64_data}\x1b\\");
    }

    let mut output = String::with_capacity(base64_data.len() + base64_data.len() / CHUNK_SIZE * 16 + params.len());
    let mut offset = 0;
    while offset < base64_data.len() {
        let end = chunk_end(base64_data, offset);
        let chunk = &base64_data[offset..end];
        if offset == 0 {
            output.push_str(&format!("\x1b_G{params},m=1;{chunk}\x1b\\"));
        } else if end >= base64_data.len() {
            output.push_str(&format!("\x1b_Gm=0;{chunk}\x1b\\"));
        } else {
            output.push_str(&format!("\x1b_Gm=1;{chunk}\x1b\\"));
        }
        offset = end;
    }
    output
}

/// Deletes a Kitty image by ID and frees its data (uppercase `I`).
pub fn delete_kitty_image(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
}

/// Deletes all visible Kitty images and frees their data (uppercase `A`).
pub fn delete_all_kitty_images() -> String {
    "\x1b_Ga=d,d=A,q=2\x1b\\".to_owned()
}

/// Deletes all visible Kitty placements while keeping their uploaded image data.
pub fn delete_all_kitty_placements() -> String {
    "\x1b_Ga=d,d=a,q=2\x1b\\".to_owned()
}

/// What [`super::render_image`] registers for a Kitty image so later frames can crop or re-place it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KittyImageMetadata {
    /// The Kitty image ID.
    pub image_id: u32,
    /// Columns covered.
    pub columns: usize,
    /// Rows covered.
    pub rows: usize,
    /// Source image width in pixels.
    pub width_px: u32,
    /// Source image height in pixels.
    pub height_px: u32,
}

/// A placement-only command that shows an already uploaded image again without its payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KittyImagePlacement {
    /// The Kitty image ID.
    pub image_id: u32,
    /// Increases each time image metadata is registered, so a re-upload can be told apart.
    pub transmission_generation: u64,
    /// Bytes of the upload commands that the placement replaces.
    pub transmission_bytes: usize,
    /// Decoded RGBA size, `width_px * height_px * 4`.
    pub estimated_decoded_bytes: u64,
    /// Rows the placement covers.
    pub rows: usize,
    /// The placement command, `ESC _G a=p,q=2,... ESC \`.
    pub sequence: String,
    /// The line with its upload commands replaced by `sequence`.
    pub replacement_line: String,
}

#[derive(Debug, Clone, Copy)]
struct RegisteredMetadata {
    metadata: KittyImageMetadata,
    transmission_generation: u64,
}

#[derive(Default)]
struct Registry {
    entries: HashMap<u32, RegisteredMetadata>,
    order: VecDeque<u32>,
    generation: u64,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(Mutex::default);

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Records the size of a Kitty image so lines that show it can be cropped and re-placed.
/// The registry keeps the 1000 most recently registered images.
pub fn register_kitty_image_metadata(metadata: KittyImageMetadata) {
    let mut registry = registry();
    registry.generation += 1;
    let transmission_generation = registry.generation;
    if registry.entries.remove(&metadata.image_id).is_some() {
        registry.order.retain(|&id| id != metadata.image_id);
    }
    registry.entries.insert(metadata.image_id, RegisteredMetadata { metadata, transmission_generation });
    registry.order.push_back(metadata.image_id);
    if registry.entries.len() > MAX_REGISTERED_IMAGES {
        if let Some(oldest) = registry.order.pop_front() {
            registry.entries.remove(&oldest);
        }
    }
}

static KITTY_CONTROLS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b_G([^;]*);").expect("valid controls pattern"));
static IMAGE_ID_CONTROL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|,)i=([0-9]+)(?:,|$)").expect("valid image id pattern"));
static ROWS_CONTROL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|,)r=([0-9]+)(?:,|$)").expect("valid rows pattern"));
static MORE_CHUNKS_CONTROL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|,)m=1(?:,|$)").expect("valid chunk pattern"));

/// Placement keys copied from an upload command into a placement-only command.
const PLACEMENT_CONTROL_KEYS: [&str; 17] =
    ["i", "p", "x", "y", "w", "h", "X", "Y", "c", "r", "C", "U", "z", "P", "Q", "H", "V"];

struct ControlsMatch<'a> {
    start: usize,
    end: usize,
    controls: &'a str,
}

fn find_controls(line: &str) -> Option<ControlsMatch<'_>> {
    let captures = KITTY_CONTROLS.captures(line)?;
    let whole = captures.get(0)?;
    Some(ControlsMatch { start: whole.start(), end: whole.end(), controls: captures.get(1)?.as_str() })
}

fn registered_from_controls(controls: &str) -> Option<RegisteredMetadata> {
    let image_id: u32 = IMAGE_ID_CONTROL.captures(controls)?.get(1)?.as_str().parse().ok()?;
    registry().entries.get(&image_id).copied()
}

fn explicit_rows(controls: &str) -> Option<usize> {
    let rows: usize = ROWS_CONTROL.captures(controls)?.get(1)?.as_str().parse().ok()?;
    (rows > 0).then_some(rows)
}

/// The registered metadata of the first Kitty image command in `line`.
pub fn get_kitty_image_metadata(line: &str) -> Option<KittyImageMetadata> {
    let found = find_controls(line)?;
    registered_from_controls(found.controls).map(|registered| registered.metadata)
}

/// The rows an image placement covers, read from its controls without scanning the payload.
pub fn get_kitty_image_placement_rows(line: &str) -> Option<usize> {
    let found = find_controls(line)?;
    explicit_rows(found.controls).or_else(|| registered_from_controls(found.controls).map(|r| r.metadata.rows))
}

/// Builds a placement-only command for an image line emitted by [`super::render_image`].
/// Returns `None` when the image is not registered or its upload commands are incomplete.
pub fn get_kitty_image_placement(line: &str) -> Option<KittyImagePlacement> {
    let found = find_controls(line)?;
    let registered = registered_from_controls(found.controls)?;

    let mut command_start = found.start;
    let mut command_controls = found.controls;
    let transmission_end = loop {
        let search_from = command_start + KITTY_PREFIX.len();
        let terminator = line.get(search_from..)?.find("\x1b\\")? + search_from;
        let end = terminator + 2;
        if !MORE_CHUNKS_CONTROL.is_match(command_controls) {
            break end;
        }
        command_start = end;
        if !line[command_start..].starts_with(KITTY_PREFIX) {
            return None;
        }
        let controls_start = command_start + KITTY_PREFIX.len();
        let controls_end = line[controls_start..].find(';')? + controls_start;
        command_controls = &line[controls_start..controls_end];
    };

    let controls: Vec<&str> = found
        .controls
        .split(',')
        .filter(|control| PLACEMENT_CONTROL_KEYS.contains(&control.split('=').next().unwrap_or("")))
        .collect();
    let sequence = format!("\x1b_Ga=p,q=2,{}\x1b\\", controls.join(","));
    let metadata = registered.metadata;
    Some(KittyImagePlacement {
        image_id: metadata.image_id,
        transmission_generation: registered.transmission_generation,
        transmission_bytes: transmission_end - found.start,
        estimated_decoded_bytes: u64::from(metadata.width_px) * u64::from(metadata.height_px) * 4,
        rows: explicit_rows(found.controls).unwrap_or(metadata.rows),
        replacement_line: format!("{}{sequence}{}", &line[..found.start], &line[transmission_end..]),
        sequence,
    })
}

/// Crops a registered Kitty image line to `visible_rows` rows after skipping `hidden_rows`.
/// Returns the line unchanged when nothing needs cropping or the image is not registered.
pub fn crop_kitty_image_line(line: &str, hidden_rows: usize, visible_rows: usize) -> String {
    let Some(found) = find_controls(line) else {
        return line.to_owned();
    };
    let Some(metadata) = registered_from_controls(found.controls).map(|registered| registered.metadata) else {
        return line.to_owned();
    };
    if hidden_rows >= metadata.rows || visible_rows == 0 {
        return line.to_owned();
    }
    let cropped_rows = visible_rows.min(metadata.rows - hidden_rows);
    if hidden_rows == 0 && cropped_rows == metadata.rows {
        return line.to_owned();
    }
    let height = u64::from(metadata.height_px);
    let rows = metadata.rows as u64;
    let source_y = height * hidden_rows as u64 / rows;
    let source_end = (height * (hidden_rows + cropped_rows) as u64).div_ceil(rows);
    let source_height = height.min(source_end).saturating_sub(source_y).max(1);
    let mut controls: Vec<String> = found
        .controls
        .split(',')
        .filter(|control| !matches!(control.as_bytes(), [b'y' | b'h' | b'r', b'=', ..]))
        .map(str::to_owned)
        .collect();
    controls.extend([format!("y={source_y}"), format!("h={source_height}"), format!("r={cropped_rows}")]);
    format!("{}\x1b_G{};{}", &line[..found.start], controls.join(","), &line[found.end..])
}
