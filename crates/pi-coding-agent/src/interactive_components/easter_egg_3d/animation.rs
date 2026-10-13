//! [`EasterEgg3dAnimation`]: the fullscreen overlay that dissolves the screen around the spinning model.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::f64::consts::PI;
use std::rc::Rc;
use std::time::{Duration, Instant};

use pi_tui::ui_loop::{self, TimerId};
use pi_tui::{
    Component, MouseResponse, Tui, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType, WeakTui, get_keybindings,
};

use super::math::{
    DEPTH, DOT_BITS, EXIT_DURATION, FLY_DURATION, FLY_START, FRAME_MS, HINT_FADE, PUZZLE_CYCLE, PUZZLE_RETURN,
    PUZZLE_START, PUZZLE_STEP, PUZZLE_STEPS, Rgb, STAR_DENSITY, STAR_TINTS, STARS_FADE, STARS_START, START_SCALE, Vec3,
    braille, clamp01, hash, mix, smooth, spin_angle, spin_phase, to_rgb,
};
use super::model::{Cell3, Model, shuffle_steps};
use super::raster::{BlockRaster, Pose};
use super::screen::ScreenCell;
use crate::interactive_components::format_key_text;
use crate::theme::{ThemeColor, theme};

/// Milliseconds on a monotonic clock, like `performance.now()`.
pub type Clock = Rc<dyn Fn() -> f64>;

/// A clock measured from the moment it was made.
pub fn monotonic_clock() -> Clock {
    let start = Instant::now();
    Rc::new(move || start.elapsed().as_secs_f64() * 1000.0)
}

/// The terminal's default colors, which the screen fades toward.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalDefaults {
    pub foreground: Rgb,
    pub background: Rgb,
}

pub(super) struct Star {
    /// Braille character with the single dot the star occupies.
    pub glyph: char,
    /// Color the star leans toward; it only ever shows a small step from the background toward it.
    pub tint: Rgb,
    /// Largest step from the background toward the tint, from 0 to 1.
    pub strength: f64,
    pub phase: f64,
    /// Flicker speed in radians per second.
    pub speed: f64,
}

pub(super) struct Exit {
    pub time: f64,
    pub yaw: f64,
    pub target_yaw: f64,
    pub offsets: Vec<Vec3>,
}

pub(super) struct Hint {
    pub row: i64,
    pub start: usize,
    pub text: Vec<char>,
    pub key_length: usize,
    pub key_color: Rgb,
    pub color: Rgb,
}

/// State the frame timer shares with the component.
struct Shared {
    clock: Clock,
    last_render: Cell<f64>,
    exit_start: Cell<Option<f64>>,
    timer: Cell<Option<TimerId>>,
    on_done: RefCell<Option<Box<dyn FnOnce()>>>,
}

impl Shared {
    fn exit_progress(&self) -> f64 {
        self.exit_start.get().map_or(0.0, |start| clamp01(((self.clock)() - start) / 1000.0 / EXIT_DURATION))
    }

    /// Stops the timer and reports the end once.
    fn finish(&self) {
        let Some(id) = self.timer.take() else {
            return;
        };
        ui_loop::clear_timer(id);
        let on_done = self.on_done.borrow_mut().take();
        if let Some(on_done) = on_done {
            on_done();
        }
    }
}

/// The 3D easter egg overlay. Escape (or `app.clear`) or a click plays the exit; a second one skips it.
pub struct EasterEgg3dAnimation {
    tui: WeakTui,
    /// The screen to dissolve, as rendered lines.
    pub(super) screen: Vec<String>,
    pub(super) model: Model,
    pub(super) foreground: Rgb,
    pub(super) background: Rgb,
    shared: Rc<Shared>,
    start_time: f64,
    pub(super) exit: Option<Exit>,
    shuffle: Option<(i64, Vec<Vec<Cell3>>)>,
    pub(super) screen_size: Option<(usize, usize)>,
    /// Star per cell index.
    pub(super) stars: Vec<Option<Star>>,
    pub(super) cells: Vec<Vec<ScreenCell>>,
    pub(super) ansi_cache: HashMap<i64, String>,
    pub(super) raster: BlockRaster,
}

impl EasterEgg3dAnimation {
    /// Starts the frame timer. `on_done` runs once the exit finished or rendering stopped for a second.
    pub fn new(
        tui: &Tui,
        screen: Vec<String>,
        model: Model,
        colors: TerminalDefaults,
        on_done: impl FnOnce() + 'static,
    ) -> Self {
        Self::with_clock(tui, screen, model, colors, monotonic_clock(), on_done)
    }

    /// Like [`EasterEgg3dAnimation::new`] with an explicit clock.
    pub fn with_clock(
        tui: &Tui,
        screen: Vec<String>,
        model: Model,
        colors: TerminalDefaults,
        clock: Clock,
        on_done: impl FnOnce() + 'static,
    ) -> Self {
        let start_time = clock();
        let shared = Rc::new(Shared {
            clock,
            last_render: Cell::new(start_time),
            exit_start: Cell::new(None),
            timer: Cell::new(None),
            on_done: RefCell::new(Some(Box::new(on_done))),
        });
        let (state, weak_tui) = (Rc::downgrade(&shared), tui.downgrade());
        let id = ui_loop::set_interval(Duration::from_secs_f64(FRAME_MS / 1000.0), move || {
            let Some(state) = state.upgrade() else {
                return;
            };
            // Also stop when no longer rendered, e.g. when pi hides all overlays on exit.
            let exited = state.exit_start.get().is_some() && state.exit_progress() >= 1.0;
            if exited || (state.clock)() - state.last_render.get() > 1000.0 {
                state.finish();
            } else if let Some(tui) = weak_tui.upgrade() {
                tui.request_render(false);
            }
        });
        shared.timer.set(Some(id));
        Self {
            tui: tui.downgrade(),
            screen,
            raster: BlockRaster::new(model.camera_distance),
            model,
            foreground: colors.foreground,
            background: colors.background,
            shared,
            start_time,
            exit: None,
            shuffle: None,
            screen_size: None,
            stars: Vec::new(),
            cells: Vec::new(),
            ansi_cache: HashMap::new(),
        }
    }

    /// Plays the exit animation. A second call skips it.
    pub fn close(&mut self) {
        if self.exit.is_some() {
            self.shared.finish();
            return;
        }
        let time = self.elapsed();
        let yaw = spin_angle(time);
        let turn = PI * 2.0;
        let offsets = self.block_offsets(time);
        self.shared.exit_start.set(Some(self.now()));
        self.exit = Some(Exit { time, yaw, target_yaw: (yaw / turn).ceil() * turn, offsets });
    }

    /// Whether the exit animation is playing.
    pub fn is_closing(&self) -> bool {
        self.exit.is_some()
    }

    /// Whether the frame timer still runs.
    pub fn is_running(&self) -> bool {
        self.shared.timer.get().is_some()
    }

    /// Stops the frame timer without reporting the end.
    pub fn dispose(&mut self) {
        if let Some(id) = self.shared.timer.take() {
            ui_loop::clear_timer(id);
        }
    }

    /// A handle that stops the frame timer, usable while the component is borrowed.
    pub fn disposer(&self) -> impl Fn() + 'static {
        let shared = Rc::downgrade(&self.shared);
        move || {
            if let Some(id) = shared.upgrade().and_then(|shared| shared.timer.take()) {
                ui_loop::clear_timer(id);
            }
        }
    }

    pub(super) fn now(&self) -> f64 {
        (self.shared.clock)()
    }

    pub(super) fn mark_rendered(&self) {
        self.shared.last_render.set(self.now());
    }

    pub(super) fn terminal_rows(&self) -> usize {
        self.tui.upgrade().map_or(1, |tui| tui.terminal().borrow().rows()).max(1)
    }

    pub(super) fn elapsed(&self) -> f64 {
        (self.now() - self.start_time) / 1000.0
    }

    pub(super) fn exit_progress(&self) -> f64 {
        self.shared.exit_progress()
    }

    /// Each block's displacement from its place in the model at `time`, in grid units.
    pub(super) fn block_offsets(&mut self, time: f64) -> Vec<Vec3> {
        let home = || vec![[0.0; 3]; self.model.blocks.len()];
        if time < PUZZLE_START {
            return home();
        }
        let cycle = ((time - PUZZLE_START) / PUZZLE_CYCLE).floor();
        let local = time - PUZZLE_START - cycle * PUZZLE_CYCLE;
        let cycle = cycle as i64;
        if self.shuffle.as_ref().is_none_or(|(cached, _)| *cached != cycle) {
            self.shuffle = Some((cycle, shuffle_steps(&self.model, cycle)));
        }
        let steps = &self.shuffle.as_ref().expect("shuffle cached above").1;
        let blocks = &self.model.blocks;
        let offset = |position: Cell3, index: usize| -> Vec3 {
            let block = blocks[index].home;
            [(position[0] - block[0]) as f64, (position[1] - block[1]) as f64, (position[2] - block[2]) as f64]
        };
        let shuffle_end = PUZZLE_STEP * PUZZLE_STEPS as f64;
        if local < shuffle_end {
            let step = ((local / PUZZLE_STEP).floor() as usize).min(PUZZLE_STEPS - 1);
            let progress = smooth((local - step as f64 * PUZZLE_STEP) / PUZZLE_STEP);
            return (0..blocks.len())
                .map(|index| {
                    let a = offset(steps[step][index], index);
                    let b = offset(steps[step + 1][index], index);
                    [0, 1, 2].map(|axis| a[axis] + (b[axis] - a[axis]) * progress)
                })
                .collect();
        }
        if local < shuffle_end + PUZZLE_RETURN {
            // All blocks fly home at once. Alternating arcs in depth keep them from passing through each other.
            let u = (local - shuffle_end) / PUZZLE_RETURN;
            let remaining = 1.0 - smooth(u);
            let arc = (PI * clamp01(u)).sin() * 0.8;
            return steps[PUZZLE_STEPS]
                .iter()
                .enumerate()
                .map(|(index, &position)| {
                    let [x, y, z] = offset(position, index);
                    let arc = if index % 2 == 0 { arc } else { -arc };
                    [x * remaining, y * remaining, z * remaining + arc]
                })
                .collect();
        }
        home()
    }

    pub(super) fn fly_progress(time: f64) -> f64 {
        smooth((time - FLY_START) / FLY_DURATION)
    }

    pub(super) fn star_alpha(time: f64) -> f64 {
        smooth((time - STARS_START) / STARS_FADE)
    }

    pub(super) fn hint_alpha(time: f64) -> f64 {
        smooth((time - FLY_START - FLY_DURATION) / HINT_FADE)
    }

    pub(super) fn pose(&self, width: usize, height: usize, progress: f64, time: f64) -> Pose {
        let Model { columns, rows, camera_distance, radius, width_share, origin, .. } = self.model;
        let (width, height, columns, rows) = (width as f64, height as f64, columns as f64, rows as f64);
        let reach = radius * (camera_distance / (camera_distance - radius));
        // Lifting off, the front face must cover exactly the half-block cells (2x2 dots per pixel) despite the
        // perspective.
        let start_scale =
            if origin.is_some() { (2.0 * (camera_distance - DEPTH / 2.0)) / camera_distance } else { START_SCALE };
        let end_scale = start_scale.max((width * 2.0 * width_share).min((height * 4.0 - 8.0) * 0.48) / reach);
        let (end_x, end_y) = (width, height * 2.0 - 2.0);
        let start_x = origin.map_or(end_x, |origin| origin.column as f64 * 2.0 + columns);
        let start_y = origin.map_or(end_y, |origin| origin.row as f64 * 4.0 + rows);
        let phase = spin_phase(time);
        Pose {
            center_x: start_x + (end_x - start_x) * progress,
            center_y: start_y + (end_y - start_y) * progress,
            // Interpolate the zoom geometrically so it feels uniform.
            scale: start_scale * (end_scale / start_scale).powf(progress),
            yaw: spin_angle(time),
            // Tilts are zero at whole turns, so the camera looks straight at the front of the reassembled logo.
            pitch: 0.3 * phase.sin() * progress,
            roll: 0.06 * (2.0 * phase).sin() * progress,
        }
    }

    pub(super) fn hint(width: usize, height: usize, alpha: f64, background: Rgb) -> Option<Hint> {
        if alpha <= 0.0 {
            return None;
        }
        let keys = get_keybindings().get_keys("tui.select.cancel");
        let key = format_key_text(keys.first().map_or("escape", String::as_str));
        let text: Vec<char> = format!("{key} to return").chars().collect();
        if text.len() > width {
            return None;
        }
        let theme = theme();
        Some(Hint {
            row: height as i64 - 2,
            start: (width - text.len()) / 2,
            key_length: key.chars().count(),
            text,
            key_color: mix(background, to_rgb(&theme.color(ThemeColor::Muted)), alpha),
            color: mix(background, to_rgb(&theme.color(ThemeColor::Dim)), alpha),
        })
    }

    /// A sparse, deterministic starfield: one braille dot in about STAR_DENSITY of all cells.
    pub(super) fn prepare_stars(width: usize, height: usize) -> Vec<Option<Star>> {
        (0..(width * height) as i64)
            .map(|index| {
                if hash(index * 3 + 0x51ed) >= STAR_DENSITY {
                    return None;
                }
                let random = |salt: i64| hash(index * 7 + salt * 0x9e37);
                Some(Star {
                    glyph: braille(DOT_BITS[(random(1) * 8.0).floor() as usize]),
                    tint: STAR_TINTS[(random(2) * STAR_TINTS.len() as f64).floor() as usize],
                    strength: 0.1 + random(3) * 0.12,
                    phase: random(4) * PI * 2.0,
                    speed: 0.4 + random(5) * 0.8,
                })
            })
            .collect()
    }
}

impl Component for EasterEgg3dAnimation {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.render_frame(width)
    }

    fn handle_input(&mut self, data: &str) {
        let keybindings = get_keybindings();
        if keybindings.matches(data, "tui.select.cancel") || keybindings.matches(data, "app.clear") {
            self.close();
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        if event.kind == TuiMouseEventType::Click {
            self.close();
        }
        Some(MouseResponse::Handled(TuiMouseEventResult { handled: true, render: Some(false), ..Default::default() }))
    }

    fn invalidate(&mut self) {
        self.screen_size = None;
        self.ansi_cache.clear();
    }
}

impl Drop for EasterEgg3dAnimation {
    fn drop(&mut self) {
        self.dispose();
    }
}
