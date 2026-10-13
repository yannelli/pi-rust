//! Block models of the 3D easter eggs and the deterministic sliding puzzle.

use super::math::{DEPTH, PUZZLE_STEPS, Rgb, hash, hypot3, js_round};
use crate::interactive_components::armin::{ARMIN_HEIGHT, ARMIN_WIDTH, is_armin_pixel};

/// Grid position of a block: column and row in the bitmap, and layer (-1, 0, 1) in depth.
pub type Cell3 = [i64; 3];

/// One block per foreground pixel; the puzzle slides blocks around.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub home: Cell3,
    pub color: Rgb,
}

/// Top-left cell of a model's half-block rendering on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelOrigin {
    pub column: usize,
    pub row: usize,
}

/// A bitmap built from one block per foreground pixel.
#[derive(Debug, Clone)]
pub struct Model {
    /// Bitmap size in pixels.
    pub columns: usize,
    pub rows: usize,
    pub blocks: Vec<Block>,
    /// Camera distance from the model's center, in pixels.
    pub camera_distance: f64,
    /// Farthest distance of any block corner from the center, with blocks on the outer depth layers.
    pub radius: f64,
    /// Largest share of the screen width the spinning model covers.
    pub width_share: f64,
    puzzle_moves: fn(f64, usize) -> usize,
    /// The model lifts off from here and replaces it; `None` to grow out of the center.
    pub origin: Option<ModelOrigin>,
}

impl Model {
    fn new(
        columns: usize,
        rows: usize,
        pixel: impl Fn(usize, usize) -> Option<Rgb>,
        camera_distance: f64,
        width_share: f64,
        puzzle_moves: fn(f64, usize) -> usize,
        origin: Option<ModelOrigin>,
    ) -> Self {
        let mut blocks = Vec::new();
        for row in 0..rows {
            for column in 0..columns {
                if let Some(color) = pixel(column, row) {
                    blocks.push(Block { home: [column as i64, row as i64, 0], color });
                }
            }
        }
        let radius = hypot3(columns as f64 / 2.0, rows as f64 / 2.0, 1.0 + DEPTH / 2.0);
        Self { columns, rows, blocks, camera_distance, radius, width_share, puzzle_moves, origin }
    }

    /// Number of blocks the puzzle moves per step, given a random number from 0 to 1.
    pub fn puzzle_moves(&self, random: f64) -> usize {
        (self.puzzle_moves)(random, self.blocks.len())
    }
}

const CORAL: Rgb = [228.0, 138.0, 122.0];
const BLUE: Rgb = [79.0, 142.0, 179.0];
const YELLOW: Rgb = [234.0, 182.0, 93.0];
const PI_LOGO_PIXELS: [&str; 4] = ["ccc.", "b.c.", "bb.y", "b..y"];

/// The pi logo, lifting off the header logo at `origin`.
pub fn pi_logo_model(origin: ModelOrigin) -> Model {
    let pixel = |column: usize, row: usize| match PI_LOGO_PIXELS[row].as_bytes()[column] {
        b'c' => Some(CORAL),
        b'b' => Some(BLUE),
        b'y' => Some(YELLOW),
        _ => None,
    };
    Model::new(4, 4, pixel, 10.0, 0.35, |random, _| if random < 0.4 { 2 } else { 1 }, Some(origin))
}

/// Armin in one color, growing out of the center.
pub fn armin_model(color: Rgb) -> Model {
    let pixel = move |column: usize, row: usize| is_armin_pixel(column, row).then_some(color);
    // Far enough that the perspective stays mild for a figure about 36 blocks tall.
    let moves = |random: f64, blocks: usize| js_round(blocks as f64 * (0.03 + random * 0.04)) as usize;
    Model::new(ARMIN_WIDTH, ARMIN_HEIGHT, pixel, 80.0, 0.45, moves, None)
}

const PUZZLE_MOVES: [Cell3; 10] =
    [[1, 0, 0], [-1, 0, 0], [0, 1, 0], [0, -1, 0], [1, 0, 0], [-1, 0, 0], [0, 1, 0], [0, -1, 0], [0, 0, 1], [0, 0, -1]];

/// Block positions after each step of one shuffle cycle, starting at home. Deterministic per cycle.
pub fn shuffle_steps(model: &Model, cycle: i64) -> Vec<Vec<Cell3>> {
    let (columns, rows) = (model.columns as i64, model.rows as i64);
    let mut positions: Vec<Cell3> = model.blocks.iter().map(|block| block.home).collect();
    let mut steps = vec![positions.clone()];
    let mut last_moved = vec![false; positions.len()];
    let mut random = cycle * 7_919 + 1;
    let mut next = || {
        let value = hash(random);
        random += 1;
        value
    };
    let key = |[x, y, z]: Cell3| (((z + 1) * rows + y) * columns + x) as usize;
    for _ in 0..PUZZLE_STEPS {
        let mut occupied = vec![false; (3 * rows * columns) as usize];
        for &position in &positions {
            occupied[key(position)] = true;
        }
        let mut next_positions = positions.clone();
        let mut moved = vec![false; positions.len()];
        let move_count = model.puzzle_moves(next());
        for _ in 0..move_count {
            let mut candidates: Vec<(usize, Cell3)> = Vec::new();
            for (block, &[x, y, z]) in positions.iter().enumerate() {
                if moved[block] {
                    continue;
                }
                for [dx, dy, dz] in PUZZLE_MOVES {
                    let target = [x + dx, y + dy, z + dz];
                    if target[0] < 0 || target[0] >= columns || target[1] < 0 || target[1] >= rows {
                        continue;
                    }
                    if target[2] < -1 || target[2] > 1 || occupied[key(target)] {
                        continue;
                    }
                    candidates.push((block, target));
                }
            }
            // Prefer blocks that did not just move, so the puzzle does not look like one block jittering.
            let fresh: Vec<(usize, Cell3)> =
                candidates.iter().copied().filter(|(block, _)| !last_moved[*block]).collect();
            let pool = if fresh.is_empty() { &candidates } else { &fresh };
            let pick = (next() * pool.len() as f64).floor() as usize;
            let Some(&(block, target)) = pool.get(pick) else {
                break;
            };
            occupied[key(target)] = true;
            next_positions[block] = target;
            moved[block] = true;
        }
        last_moved = moved;
        positions = next_positions;
        steps.push(positions.clone());
    }
    steps
}
