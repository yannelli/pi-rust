//! [`ArminComponent`]: Armin says hi! A fun easter egg with animated XBM art (upstream `armin.ts`).

use std::cell::{Cell, RefCell};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::rc::Rc;
use std::time::Duration;

use pi_tui::ui_loop::{self, TimerId};
use pi_tui::{Component, Tui};

use crate::theme::{ThemeColor, theme};

/// XBM image: 31x36 pixels, LSB first, 1=background, 0=foreground.
pub const ARMIN_WIDTH: usize = 31;
pub const ARMIN_HEIGHT: usize = 36;
const WIDTH: usize = ARMIN_WIDTH;
const HEIGHT: usize = ARMIN_HEIGHT;
#[rustfmt::skip]
const BITS: [u8; 144] = [
    0xff, 0xff, 0xff, 0x7f, 0xff, 0xf0, 0xff, 0x7f, 0xff, 0xed, 0xff, 0x7f, 0xff, 0xdb, 0xff, 0x7f, 0xff, 0xb7, 0xff,
    0x7f, 0xff, 0x77, 0xfe, 0x7f, 0x3f, 0xf8, 0xfe, 0x7f, 0xdf, 0xff, 0xfe, 0x7f, 0xdf, 0x3f, 0xfc, 0x7f, 0x9f, 0xc3,
    0xfb, 0x7f, 0x6f, 0xfc, 0xf4, 0x7f, 0xf7, 0x0f, 0xf7, 0x7f, 0xf7, 0xff, 0xf7, 0x7f, 0xf7, 0xff, 0xe3, 0x7f, 0xf7,
    0x07, 0xe8, 0x7f, 0xef, 0xf8, 0x67, 0x70, 0x0f, 0xff, 0xbb, 0x6f, 0xf1, 0x00, 0xd0, 0x5b, 0xfd, 0x3f, 0xec, 0x53,
    0xc1, 0xff, 0xef, 0x57, 0x9f, 0xfd, 0xee, 0x5f, 0x9f, 0xfc, 0xae, 0x5f, 0x1f, 0x78, 0xac, 0x5f, 0x3f, 0x00, 0x50,
    0x6c, 0x7f, 0x00, 0xdc, 0x77, 0xff, 0xc0, 0x3f, 0x78, 0xff, 0x01, 0xf8, 0x7f, 0xff, 0x03, 0x9c, 0x78, 0xff, 0x07,
    0x8c, 0x7c, 0xff, 0x0f, 0xce, 0x78, 0xff, 0xff, 0xcf, 0x7f, 0xff, 0xff, 0xcf, 0x78, 0xff, 0xff, 0xdf, 0x78, 0xff,
    0xff, 0xdf, 0x7d, 0xff, 0xff, 0x3f, 0x7e, 0xff, 0xff, 0xff, 0x7f,
];
const BYTES_PER_ROW: usize = WIDTH.div_ceil(8);
/// Half-block rendering packs two pixel rows per line.
const DISPLAY_HEIGHT: usize = HEIGHT.div_ceil(2);
const NOISE: [&str; 7] = [" ", "░", "▒", "▓", "█", "▀", "▄"];

/// Pixel at (x, y): true = foreground, false = background.
pub fn is_armin_pixel(x: usize, y: usize) -> bool {
    if y >= HEIGHT {
        return false;
    }
    let byte = BITS[y * BYTES_PER_ROW + x / 8];
    (byte >> (x % 8)) & 1 == 0
}

fn cell_char(x: usize, row: usize) -> &'static str {
    match (is_armin_pixel(x, row * 2), is_armin_pixel(x, row * 2 + 1)) {
        (true, true) => "█",
        (true, false) => "▀",
        (false, true) => "▄",
        (false, false) => " ",
    }
}

type Grid = Vec<Vec<&'static str>>;

fn build_final_grid() -> Grid {
    (0..DISPLAY_HEIGHT).map(|row| (0..WIDTH).map(|x| cell_char(x, row)).collect()).collect()
}

fn empty_grid() -> Grid {
    vec![vec![" "; WIDTH]; DISPLAY_HEIGHT]
}

/// How the picture appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArminEffect {
    Typewriter,
    Scanline,
    Rain,
    Fade,
    Crt,
    Glitch,
    Dissolve,
}

impl ArminEffect {
    pub const ALL: [ArminEffect; 7] = [
        ArminEffect::Typewriter,
        ArminEffect::Scanline,
        ArminEffect::Rain,
        ArminEffect::Fade,
        ArminEffect::Crt,
        ArminEffect::Glitch,
        ArminEffect::Dissolve,
    ];
}

/// A uniform random number from 0 (inclusive) to 1 (exclusive), like `Math.random`.
pub type RandomFn = Box<dyn FnMut() -> f64>;

/// `Math.random` stand-in: xorshift64* seeded from the process's hash keys.
pub fn default_random() -> RandomFn {
    let mut state = RandomState::new().build_hasher().finish() | 1;
    Box::new(move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let value = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (value >> 11) as f64 / (1u64 << 53) as f64
    })
}

struct RainDrop {
    y: i64,
    settled: usize,
}

enum EffectState {
    Typewriter { pos: usize },
    Scanline { row: usize },
    Rain { drops: Vec<RainDrop> },
    Fade { positions: Vec<(usize, usize)>, idx: usize },
    Crt { expansion: usize },
    Glitch { phase: usize, glitch_frames: usize },
    Dissolve { positions: Vec<(usize, usize)>, idx: usize },
}

fn random_index(random: &mut RandomFn, len: usize) -> usize {
    ((random() * len as f64).floor() as usize).min(len.saturating_sub(1))
}

fn shuffled_positions(random: &mut RandomFn) -> Vec<(usize, usize)> {
    let mut positions: Vec<(usize, usize)> =
        (0..DISPLAY_HEIGHT).flat_map(|row| (0..WIDTH).map(move |x| (row, x))).collect();
    for i in (1..positions.len()).rev() {
        let j = random_index(random, i + 1);
        positions.swap(i, j);
    }
    positions
}

struct Animation {
    effect: ArminEffect,
    final_grid: Grid,
    current_grid: Grid,
    state: EffectState,
    grid_version: u64,
    random: RandomFn,
}

impl Animation {
    fn new(effect: ArminEffect, mut random: RandomFn) -> Self {
        let mut current_grid = empty_grid();
        let state = match effect {
            ArminEffect::Typewriter => EffectState::Typewriter { pos: 0 },
            ArminEffect::Scanline => EffectState::Scanline { row: 0 },
            ArminEffect::Rain => {
                let drops = (0..WIDTH)
                    .map(|_| RainDrop { y: -((random() * (DISPLAY_HEIGHT * 2) as f64).floor() as i64), settled: 0 })
                    .collect();
                EffectState::Rain { drops }
            }
            ArminEffect::Fade => EffectState::Fade { positions: shuffled_positions(&mut random), idx: 0 },
            ArminEffect::Crt => EffectState::Crt { expansion: 0 },
            ArminEffect::Glitch => EffectState::Glitch { phase: 0, glitch_frames: 8 },
            ArminEffect::Dissolve => {
                // Start with random noise, then resolve pixels in shuffled order.
                current_grid = (0..DISPLAY_HEIGHT)
                    .map(|_| (0..WIDTH).map(|_| NOISE[random_index(&mut random, NOISE.len())]).collect())
                    .collect();
                EffectState::Dissolve { positions: shuffled_positions(&mut random), idx: 0 }
            }
        };
        Self { effect, final_grid: build_final_grid(), current_grid, state, grid_version: 0, random }
    }

    /// Advances one frame; true when the effect is done.
    fn tick(&mut self) -> bool {
        let Self { final_grid, current_grid, state, random, .. } = self;
        match state {
            EffectState::Typewriter { pos } => {
                for _ in 0..3 {
                    let (row, x) = (*pos / WIDTH, *pos % WIDTH);
                    if row >= DISPLAY_HEIGHT {
                        return true;
                    }
                    current_grid[row][x] = final_grid[row][x];
                    *pos += 1;
                }
                false
            }
            EffectState::Scanline { row } => {
                if *row >= DISPLAY_HEIGHT {
                    return true;
                }
                current_grid[*row].clone_from(&final_grid[*row]);
                *row += 1;
                false
            }
            EffectState::Rain { drops } => tick_rain(drops, final_grid, current_grid, random),
            EffectState::Fade { positions, idx } => reveal(positions, idx, 15, final_grid, current_grid),
            EffectState::Crt { expansion } => {
                let mid_row = DISPLAY_HEIGHT / 2;
                *current_grid = empty_grid();
                let top = mid_row.saturating_sub(*expansion);
                let bottom = (mid_row + *expansion).min(DISPLAY_HEIGHT - 1);
                for row in top..=bottom {
                    current_grid[row].clone_from(&final_grid[row]);
                }
                *expansion += 1;
                *expansion > DISPLAY_HEIGHT
            }
            EffectState::Glitch { phase, glitch_frames } => {
                if *phase < *glitch_frames {
                    *current_grid = final_grid.iter().map(|row| glitch_row(row, final_grid, random)).collect();
                    *phase += 1;
                    return false;
                }
                current_grid.clone_from(final_grid);
                true
            }
            EffectState::Dissolve { positions, idx } => reveal(positions, idx, 20, final_grid, current_grid),
        }
    }
}

fn reveal(positions: &[(usize, usize)], idx: &mut usize, per_frame: usize, final_grid: &Grid, grid: &mut Grid) -> bool {
    for _ in 0..per_frame {
        let Some(&(row, x)) = positions.get(*idx) else {
            return true;
        };
        grid[row][x] = final_grid[row][x];
        *idx += 1;
    }
    false
}

fn glitch_row(row: &[&'static str], final_grid: &Grid, random: &mut RandomFn) -> Vec<&'static str> {
    let offset = (random() * 7.0).floor() as i64 - 3;
    // Random horizontal offset: a positive offset rotates left, a negative one rotates right.
    if random() < 0.3 {
        let mut shifted = row.to_vec();
        if offset > 0 {
            shifted.rotate_left(offset as usize);
        } else {
            shifted.rotate_right(offset.unsigned_abs() as usize);
        }
        return shifted;
    }
    // Random vertical swap.
    if random() < 0.2 {
        return final_grid[random_index(random, DISPLAY_HEIGHT)].clone();
    }
    row.to_vec()
}

fn tick_rain(drops: &mut [RainDrop], final_grid: &Grid, grid: &mut Grid, random: &mut RandomFn) -> bool {
    let mut all_settled = true;
    *grid = empty_grid();
    let height = DISPLAY_HEIGHT as i64;
    for (x, drop) in drops.iter_mut().enumerate() {
        // Draw settled pixels.
        for row in DISPLAY_HEIGHT.saturating_sub(drop.settled)..DISPLAY_HEIGHT {
            grid[row][x] = final_grid[row][x];
        }
        if drop.settled >= DISPLAY_HEIGHT {
            continue;
        }
        all_settled = false;
        // The target row is the lowest non-space pixel above the settled ones.
        let target_row = (0..DISPLAY_HEIGHT - drop.settled)
            .rev()
            .find(|&row| final_grid[row][x] != " ")
            .map_or(-1, |row| row as i64);
        drop.y += 1;
        if drop.y >= 0 && drop.y < height {
            if target_row >= 0 && drop.y >= target_row {
                drop.settled = (height - target_row) as usize;
                drop.y = -((random() * 5.0).floor() as i64) - 1;
            } else {
                grid[drop.y as usize][x] = "▓";
            }
        }
    }
    all_settled
}

/// The `/arminsayshi` picture, drawn by a randomly chosen effect at 30 fps (60 for the glitch).
pub struct ArminComponent {
    animation: Rc<RefCell<Animation>>,
    timer: Rc<Cell<Option<TimerId>>>,
    cached_lines: Vec<String>,
    cached_width: usize,
    cached_version: Option<u64>,
}

impl ArminComponent {
    /// Starts the animation with a random effect; each frame requests a render from `tui`.
    pub fn new(tui: &Tui) -> Self {
        Self::with_random(tui, None, default_random())
    }

    /// Like [`ArminComponent::new`] with a chosen effect (or a random one) and a random source.
    pub fn with_random(tui: &Tui, effect: Option<ArminEffect>, mut random: RandomFn) -> Self {
        let effect = effect.unwrap_or_else(|| ArminEffect::ALL[random_index(&mut random, ArminEffect::ALL.len())]);
        let animation = Rc::new(RefCell::new(Animation::new(effect, random)));
        let timer: Rc<Cell<Option<TimerId>>> = Rc::default();
        let fps = if effect == ArminEffect::Glitch { 60.0 } else { 30.0 };
        let (state, handle, weak_tui) = (animation.clone(), timer.clone(), tui.downgrade());
        let id = ui_loop::set_interval(Duration::from_secs_f64(1.0 / fps), move || {
            let done = advance(&state);
            if let Some(tui) = weak_tui.upgrade() {
                tui.request_render(false);
            }
            if done && let Some(id) = handle.take() {
                ui_loop::clear_timer(id);
            }
        });
        timer.set(Some(id));
        Self { animation, timer, cached_lines: Vec::new(), cached_width: 0, cached_version: None }
    }

    pub fn effect(&self) -> ArminEffect {
        self.animation.borrow().effect
    }

    /// Whether the frame timer still runs.
    pub fn is_animating(&self) -> bool {
        self.timer.get().is_some()
    }

    /// Runs one animation frame without waiting for the timer; true when the effect is done.
    pub fn step(&mut self) -> bool {
        advance(&self.animation)
    }

    pub fn dispose(&mut self) {
        if let Some(id) = self.timer.take() {
            ui_loop::clear_timer(id);
        }
    }

    /// A handle that stops the animation, usable while the component is borrowed.
    pub fn disposer(&self) -> impl Fn() + 'static {
        let timer = self.timer.clone();
        move || {
            if let Some(id) = timer.take() {
                ui_loop::clear_timer(id);
            }
        }
    }
}

fn advance(animation: &RefCell<Animation>) -> bool {
    let mut animation = animation.borrow_mut();
    let done = animation.tick();
    animation.grid_version += 1;
    done
}

fn padded_line(content: &str, content_width: usize, width: usize) -> String {
    let pad_right = width.saturating_sub(1 + content_width);
    format!(" {}{}", theme().fg(ThemeColor::Accent, content), " ".repeat(pad_right))
}

impl Component for ArminComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let animation = self.animation.borrow();
        if width == self.cached_width && self.cached_version == Some(animation.grid_version) {
            return self.cached_lines.clone();
        }
        let available_width = width.saturating_sub(1);
        let mut lines: Vec<String> = animation
            .current_grid
            .iter()
            .map(|row| {
                let clipped: String = row.iter().take(available_width).copied().collect();
                padded_line(&clipped, row.len().min(available_width), width)
            })
            .collect();
        let message = "ARMIN SAYS HI";
        lines.push(padded_line(message, message.len(), width));
        self.cached_width = width;
        self.cached_version = Some(animation.grid_version);
        self.cached_lines = lines;
        self.cached_lines.clone()
    }

    fn invalidate(&mut self) {
        self.cached_width = 0;
    }
}

impl Drop for ArminComponent {
    fn drop(&mut self) {
        self.dispose();
    }
}
