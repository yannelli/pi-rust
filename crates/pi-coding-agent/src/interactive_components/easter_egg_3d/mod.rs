//! Fullscreen 3D easter eggs (upstream `easter-egg-3d.ts`, `easter-egg-3d.lazy.ts`): the pi logo and Armin,
//! one block per pixel, ray cast per braille dot (a half-block pixel is 2x2 dots). The screen dissolves into
//! braille dust while the model spins and plays a sliding puzzle; leaving plays the timeline backwards.

mod animation;
mod frame;
mod halo;
mod math;
mod model;
mod raster;
mod screen;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use pi_tui::{OverlayAnchor, OverlayHandle, OverlayOptions, SizeValue, Tui, component};

pub use animation::{Clock, EasterEgg3dAnimation, TerminalDefaults, monotonic_clock};
pub use math::{Rgb, hash};
pub use model::{Block, Model, ModelOrigin, armin_model, pi_logo_model, shuffle_steps};
pub use screen::{ScreenCell, glyph_ink, parse_screen};

use crate::theme::{ThemeAppearance, ThemeColor, theme};

/// Which easter egg to play. The pi logo lifts off the header logo, whose top-left cell is at `column`, `row`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EasterEgg3d {
    PiLogo { column: usize, row: usize },
    Armin,
}

thread_local! {
    static PLAYING: Cell<bool> = const { Cell::new(false) };
}

/// Plays an easter egg. Only fullscreen mode can show it, because it dissolves the rendered screen, which is
/// captured first. Returns false when it cannot play.
pub fn play_easter_egg_3d(tui: &Tui, egg: EasterEgg3d) -> bool {
    if !tui.is_viewport() {
        return false;
    }
    if tui.has_overlay() {
        return true;
    }
    let screen = tui.get_screen_lines();
    start(tui, screen, egg);
    true
}

/// Plays the 3D pi logo, lifting off the header logo whose top-left cell is at `column`, `row`.
pub fn play_pi_logo_3d(tui: &Tui, column: usize, row: usize) {
    play_easter_egg_3d(tui, EasterEgg3d::PiLogo { column, row });
}

/// Plays the 3D Armin. Returns false when it cannot play, so the caller can fall back to the inline version.
pub fn play_armin_3d(tui: &Tui) -> bool {
    play_easter_egg_3d(tui, EasterEgg3d::Armin)
}

/// Shows the animation as a fullscreen overlay until it is dismissed. The overlay takes focus and mouse input
/// and returns focus when hidden, so the rest of the UI keeps running underneath.
fn start(tui: &Tui, screen: Vec<String>, egg: EasterEgg3d) {
    if PLAYING.get() {
        return;
    }
    PLAYING.set(true);
    let weak = tui.downgrade();
    // Fading needs the terminal's actual default colors; the theme only knows its own.
    let on_colors = move |reported: pi_tui::terminal_colors::TerminalColors| {
        let Some(tui) = weak.upgrade() else {
            PLAYING.set(false);
            return;
        };
        let active = theme();
        let dark = active.appearance() == ThemeAppearance::Dark;
        let rgb =
            |color: pi_tui::terminal_colors::RgbColor| [f64::from(color.r), f64::from(color.g), f64::from(color.b)];
        let colors = TerminalDefaults {
            foreground: reported.foreground.map_or_else(|| math::to_rgb(&active.color(ThemeColor::Text)), rgb),
            background: reported.background.map_or(if dark { [0.0; 3] } else { [255.0; 3] }, rgb),
        };
        if tui.has_overlay() {
            PLAYING.set(false);
            return;
        }
        let model = match egg {
            EasterEgg3d::Armin => armin_model(math::to_rgb(&active.color(ThemeColor::Accent))),
            EasterEgg3d::PiLogo { column, row } => pi_logo_model(ModelOrigin { column, row }),
        };
        let overlay: Rc<RefCell<Option<OverlayHandle>>> = Rc::default();
        let handle = overlay.clone();
        let animation = EasterEgg3dAnimation::new(&tui, screen, model, colors, move || {
            PLAYING.set(false);
            let handle = handle.borrow_mut().take();
            if let Some(handle) = handle {
                handle.hide();
            }
        });
        let options = OverlayOptions {
            anchor: Some(OverlayAnchor::TopLeft),
            width: Some(SizeValue::Percent(100.0)),
            max_height: Some(SizeValue::Percent(100.0)),
            ..OverlayOptions::default()
        };
        *overlay.borrow_mut() = Some(tui.show_overlay(component(animation), options));
    };
    tui.query_terminal_colors(Duration::from_millis(100), on_colors, None);
}

/// Whether an easter egg is playing on this thread.
pub fn is_playing() -> bool {
    PLAYING.get()
}
