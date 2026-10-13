//! Numeric helpers, timing constants, and the spin of the 3D easter eggs.

use std::f64::consts::PI;

use pi_tui::colors::{Color, color_to_rgb};

pub type Rgb = [f64; 3];
pub type Vec3 = [f64; 3];

pub const DEPTH: f64 = 0.7;
pub const FLY_START: f64 = 0.1;
pub const FLY_DURATION: f64 = 1.3;
/// Braille dots per pixel when a model without an origin appears, growing from a speck at the center.
pub const START_SCALE: f64 = 0.1;

// The sliding puzzle: after the model spun for a while, blocks slide into free neighboring cells, a few at a
// time. Each cycle shuffles, flies every block back home, and holds the model briefly.
pub const PUZZLE_START: f64 = FLY_START + FLY_DURATION + 3.0;
pub const PUZZLE_STEP: f64 = 0.3;
pub const PUZZLE_STEPS: usize = 12;
pub const PUZZLE_RETURN: f64 = 1.0;
pub const PUZZLE_HOLD: f64 = 0.6;
pub const PUZZLE_CYCLE: f64 = PUZZLE_STEP * PUZZLE_STEPS as f64 + PUZZLE_RETURN + PUZZLE_HOLD;

pub const FRAME_MS: f64 = 1000.0 / 30.0;
pub const DUST_DURATION: f64 = 0.35;
pub const WAVE_SPREAD: f64 = 0.55;
pub const WAVE_JITTER: f64 = 0.12;
pub const DISSOLVE_END: f64 = FLY_START + WAVE_SPREAD + WAVE_JITTER + DUST_DURATION;
pub const BACKGROUND_FADE: f64 = 0.5;
pub const EXIT_DURATION: f64 = 1.1;
// A faint starfield fades in behind the logo once the screen has dissolved.
pub const STARS_START: f64 = 2.5;
pub const STARS_FADE: f64 = 2.0;
pub const STAR_DENSITY: f64 = 0.018;
// Star tints: neutral, pale blue, pale gold, and pale rose, each mixed half with the terminal's foreground.
pub const STAR_TINTS: [Rgb; 4] =
    [[255.0, 255.0, 255.0], [170.0, 195.0, 255.0], [255.0, 225.0, 170.0], [255.0, 190.0, 205.0]];
pub const LOGO_COLOR_STEP: f64 = 5.0;
// On light backgrounds the logo gets a halo: its colors and coverage blurred into the cell backgrounds,
// fading out over HALO_RADIUS_X columns and HALO_RADIUS_Y rows (cells are about twice as tall as wide).
pub const LIGHT_HALO_STRENGTH: f64 = 0.3;
pub const HALO_RADIUS_X: i64 = 6;
pub const HALO_RADIUS_Y: i64 = 3;
pub const HALO_LEVELS: f64 = 32.0;
pub const STAR_ALPHA_LEVELS: f64 = 8.0;
pub const STAR_FLICKER_LEVELS: f64 = 3.0;
pub const HINT_FADE: f64 = 0.5;

/// Braille dot bits, indexed by row * 2 + column within the 2x4 cell.
pub const DOT_BITS: [u8; 8] = [0x01, 0x08, 0x02, 0x10, 0x04, 0x20, 0x40, 0x80];

/// The braille character with the given dot bits.
pub fn braille(bits: u8) -> char {
    char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ')
}

pub fn hypot3(a: f64, b: f64, c: f64) -> f64 {
    (a * a + b * b + c * c).sqrt()
}

pub fn normalize(v: Vec3) -> Vec3 {
    let length = hypot3(v[0], v[1], v[2]);
    [v[0] / length, v[1] / length, v[2] / length]
}

pub fn light_direction() -> Vec3 {
    normalize([-0.45, -0.6, 0.75])
}

pub fn half_vector() -> Vec3 {
    let light = light_direction();
    normalize([light[0], light[1], light[2] + 1.0])
}

pub fn clamp01(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

/// Quintic smoothstep.
pub fn smooth(value: f64) -> f64 {
    let t = clamp01(value);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

pub fn ease_out(value: f64) -> f64 {
    let t = 1.0 - clamp01(value);
    1.0 - t * t * t
}

pub fn mix(a: Rgb, b: Rgb, amount: f64) -> Rgb {
    [a[0] + (b[0] - a[0]) * amount, a[1] + (b[1] - a[1]) * amount, a[2] + (b[2] - a[2]) * amount]
}

/// `Math.round`: halves round up.
pub fn js_round(value: f64) -> f64 {
    let floor = value.floor();
    if value - floor >= 0.5 { floor + 1.0 } else { floor }
}

/// JavaScript `ToInt32` of a number.
pub fn to_int32(value: f64) -> i32 {
    if value.is_finite() { (value.trunc() as i64) as i32 } else { 0 }
}

/// A deterministic number from 0 to 1 for an integer, like upstream's `Math.imul` mixer.
pub fn hash(value: i64) -> f64 {
    let mut x = (value as u32 ^ 0x9e37_79b9).wrapping_mul(0x85eb_ca6b);
    x ^= x >> 13;
    x = x.wrapping_mul(0xc2b2_ae35);
    x ^= x >> 16;
    f64::from(x) / 4_294_967_296.0
}

/// Whether a background is light, by relative luminance.
pub fn is_light([r, g, b]: Rgb) -> bool {
    0.2126 * r + 0.7152 * g + 0.0722 * b > 128.0
}

pub fn to_rgb(color: &Color) -> Rgb {
    let rgb = color_to_rgb(color);
    [f64::from(rgb.r), f64::from(rgb.g), f64::from(rgb.b)]
}

/// Row-major 3x3 rotation matrix for yaw (y), then pitch (x), then roll (z).
pub fn rotation(yaw: f64, pitch: f64, roll: f64) -> [f64; 9] {
    let (sy, cy, sx, cx, sz, cz) = (yaw.sin(), yaw.cos(), pitch.sin(), pitch.cos(), roll.sin(), roll.cos());
    // Rz * Rx * Ry
    let ry = [cy, 0.0, sy, 0.0, 1.0, 0.0, -sy, 0.0, cy];
    let rx = [1.0, 0.0, 0.0, 0.0, cx, -sx, 0.0, sx, cx];
    let rz = [cz, -sz, 0.0, sz, cz, 0.0, 0.0, 0.0, 1.0];
    multiply(&rz, &multiply(&rx, &ry))
}

fn multiply(a: &[f64; 9], b: &[f64; 9]) -> [f64; 9] {
    let mut result = [0.0; 9];
    for row in 0..3 {
        for column in 0..3 {
            result[row * 3 + column] =
                a[row * 3] * b[column] + a[row * 3 + 1] * b[3 + column] + a[row * 3 + 2] * b[6 + column];
        }
    }
    result
}

// The spin is locked to the puzzle: one turn per cycle, facing the camera in the middle of each hold.
const SPIN_RAMP: f64 = 1.4;
const SPIN_SPEED: f64 = (PI * 2.0) / PUZZLE_CYCLE;
// How much the spin slows down while facing the camera (and speeds up while facing away), from 0 to 1.
const SPIN_LINGER: f64 = 0.6;
const FIRST_FRONT_VIEW: f64 = PUZZLE_START + PUZZLE_STEP * PUZZLE_STEPS as f64 + PUZZLE_RETURN + PUZZLE_HOLD / 2.0;

/// Uniform spin that accelerates from rest over SPIN_RAMP, then turns at SPIN_SPEED.
fn base_spin_phase(time: f64) -> f64 {
    let elapsed = time - FLY_START;
    if elapsed <= 0.0 {
        return 0.0;
    }
    let u = elapsed / SPIN_RAMP;
    // Integral of the quintic smoothstep ramp.
    if u < 1.0 {
        return SPIN_SPEED * SPIN_RAMP * (u.powi(6) - 3.0 * u.powi(5) + 2.5 * u.powi(4));
    }
    SPIN_SPEED * (SPIN_RAMP * 0.5 + elapsed - SPIN_RAMP)
}

/// Extra rotation added during the flight, so the phase is a whole number of turns at every assembled front view.
fn spin_alignment() -> f64 {
    PI * 2.0 - (base_spin_phase(FIRST_FRONT_VIEW) % (PI * 2.0))
}

/// Spin phase: whole turns exactly when the reassembled logo faces the camera.
pub fn spin_phase(time: f64) -> f64 {
    base_spin_phase(time) + spin_alignment() * smooth((time - FLY_START) / FLY_DURATION)
}

/// Yaw of the logo. It matches the phase at whole turns but lingers there, so the logo reads from the front.
pub fn spin_angle(time: f64) -> f64 {
    let phase = spin_phase(time);
    phase - SPIN_LINGER * phase.sin()
}
