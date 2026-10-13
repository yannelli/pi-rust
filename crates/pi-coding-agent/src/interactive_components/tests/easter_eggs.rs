//! Armin, the 3D easter eggs, and the Earendil announcement. Frames are compared with fixtures rendered by
//! upstream `armin.ts` and `easter-egg-3d.ts` (dark theme, truecolor) with the same clock and random numbers.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use pi_tui::colors::TerminalColorMode;
use pi_tui::testing::VirtualTerminal;
use pi_tui::{
    Component, MouseResponse, Tui, TuiAltScreenOptions, TuiMouseButton, TuiMouseEvent, TuiMouseEventType, TuiOptions,
    ui_loop,
};
use serde_json::Value;

use crate::interactive_components::armin::{
    ARMIN_HEIGHT, ARMIN_WIDTH, ArminComponent, ArminEffect, RandomFn, is_armin_pixel,
};
use crate::interactive_components::earendil_announcement::{BLOG_URL, EarendilAnnouncementComponent};
use crate::interactive_components::easter_egg_3d::{
    Clock, EasterEgg3d, EasterEgg3dAnimation, ModelOrigin, TerminalDefaults, armin_model, glyph_ink, hash, is_playing,
    parse_screen, pi_logo_model, play_armin_3d, play_easter_egg_3d, play_pi_logo_3d, shuffle_steps,
};
use crate::interactive_components::test_support::{Calls, Harness, keys, plain, setup};
use crate::theme::tests::global_state;
use crate::theme::{create_theme, load_theme_json, set_theme_instance};

static FIXTURES: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("fixtures/easter_eggs.json")).expect("fixture json"));

fn strings(value: &Value) -> Vec<String> {
    value.as_array().expect("array").iter().map(|line| line.as_str().expect("string").to_string()).collect()
}

/// The dark theme in truecolor, as the fixtures were rendered.
fn dark_truecolor() {
    let json = load_theme_json("dark").expect("dark theme");
    set_theme_instance(Arc::new(create_theme(&json, Some(TerminalColorMode::Truecolor), None).expect("theme")));
}

/// The random sequence of the fixtures: the effect pick, then a floating point LCG.
fn fixture_random(index: usize) -> RandomFn {
    let mut seed = 12_345.0 + index as f64;
    let mut first = true;
    Box::new(move || {
        if std::mem::take(&mut first) {
            return (index as f64 + 0.5) / 7.0;
        }
        seed = (seed * 1_103_515_245.0 + 12_345.0) % 2_147_483_648.0;
        seed / 2_147_483_648.0
    })
}

fn trimmed(lines: Vec<String>) -> Vec<String> {
    plain(&lines).into_iter().map(|line| line.trim_end().to_string()).collect()
}

fn test_tui(columns: usize, rows: usize) -> (Rc<RefCell<VirtualTerminal>>, Tui) {
    let terminal = VirtualTerminal::shared(columns, rows);
    (terminal.clone(), Tui::new_main_screen(terminal, TuiOptions::default()))
}

#[test]
fn armin_effects_match_upstream_frames() {
    let _theme = global_state();
    let _kb = setup();
    dark_truecolor();
    let (_terminal, tui) = test_tui(40, 20);
    for (index, effect) in ArminEffect::ALL.into_iter().enumerate() {
        let name = ["typewriter", "scanline", "rain", "fade", "crt", "glitch", "dissolve"][index];
        let mut armin = ArminComponent::with_random(&tui, None, fixture_random(index));
        armin.dispose();
        assert_eq!(armin.effect(), effect);
        let expected = FIXTURES["armin_effects"][name].as_array().expect("frames");
        let mut frames = expected.iter();
        let mut check = |armin: &mut ArminComponent, ticks: usize, done: bool| {
            let frame = frames.next().expect("frame");
            assert_eq!(frame["ticks"], ticks, "{name}");
            assert_eq!(frame["done"], done, "{name} after {ticks} ticks");
            assert_eq!(trimmed(armin.render(40)), strings(&frame["lines"]), "{name} after {ticks} ticks");
        };
        check(&mut armin, 0, false);
        let (mut ticks, mut done) = (0, false);
        while ticks < 400 {
            done = armin.step();
            ticks += 1;
            if [1, 3, 8, 20].contains(&ticks) {
                check(&mut armin, ticks, done);
            }
            if done {
                break;
            }
        }
        check(&mut armin, ticks, done);
    }
}

#[test]
fn armin_renders_accent_lines_padded_to_width() {
    let _theme = global_state();
    let _kb = setup();
    dark_truecolor();
    let (_terminal, tui) = test_tui(40, 20);
    let mut armin = ArminComponent::with_random(&tui, Some(ArminEffect::Scanline), fixture_random(1));
    while !armin.step() {}
    let lines = armin.render(40);
    assert_eq!(lines[0], FIXTURES["armin_raw_line"].as_str().unwrap());
    assert_eq!(lines.len(), ARMIN_HEIGHT / 2 + 1);
    let narrow = trimmed(armin.render(10));
    let (message, image) = narrow.split_last().unwrap();
    assert!(image.iter().all(|line| line.chars().count() <= 10), "{narrow:?}");
    assert_eq!(image[0], "         ▄");
    assert_eq!(message, " ARMIN SAYS HI");
}

#[test]
fn armin_bitmap_has_expected_corners() {
    assert_eq!((ARMIN_WIDTH, ARMIN_HEIGHT), (31, 36));
    assert!(!is_armin_pixel(0, 0));
    assert!(is_armin_pixel(9, 1));
    assert!(!is_armin_pixel(30, 35));
    assert!(!is_armin_pixel(5, 36));
}

#[test]
fn armin_timer_drives_frames_and_stops() {
    let _theme = global_state();
    let harness =
        Harness::new(40, 30, |tui| ArminComponent::with_random(tui, Some(ArminEffect::Scanline), fixture_random(1)));
    assert!(harness.component.borrow().is_animating());
    harness.run_for(Duration::from_millis(150));
    let rows = harness.viewport().iter().filter(|line| !line.trim().is_empty()).count();
    assert!((2..19).contains(&rows), "{}", harness.screen());
    harness.run_for(Duration::from_millis(900));
    assert!(!harness.component.borrow().is_animating());
    assert!(harness.screen().contains("ARMIN SAYS HI"));
    assert!(harness.screen().contains("▀▄▄"), "{}", harness.screen());
}

#[test]
fn armin_dispose_and_disposer_stop_the_timer() {
    let _theme = global_state();
    let (_terminal, tui) = test_tui(40, 20);
    let mut armin = ArminComponent::with_random(&tui, Some(ArminEffect::Typewriter), fixture_random(0));
    armin.dispose();
    assert!(!armin.is_animating());
    let before = trimmed(armin.render(40));
    ui_loop::run_for(Duration::from_millis(80)).unwrap();
    assert_eq!(trimmed(armin.render(40)), before);
    let other = ArminComponent::with_random(&tui, Some(ArminEffect::Rain), fixture_random(2));
    let stop = other.disposer();
    stop();
    assert!(!other.is_animating());
    let random = ArminComponent::new(&tui);
    assert!(ArminEffect::ALL.contains(&random.effect()));
}

type TestClock = Rc<Cell<f64>>;

fn clock() -> (TestClock, Clock) {
    let now = Rc::new(Cell::new(1000.0));
    let handle = now.clone();
    (now, Rc::new(move || handle.get()))
}

const SCREEN: [&str; 7] = [
    "\x1b[38;2;228;138;122m██████\x1b[0m pi v1.2.3",
    "\x1b[38;5;33m██  ██\x1b[39m \x1b[1mbold\x1b[22m and \x1b[2mdim\x1b[22m text",
    "\x1b[48;2;40;40;60m on a panel \x1b[49m plain, wide 日本語 ok",
    "\x1b[7minverse\x1b[27m \x1b[31mred\x1b[0m \x1b]8;;https://x.y\x07link\x1b]8;;\x07 ─── ...",
    "",
    "$ cargo test -- --nocapture; echo done",
    "\x1b[90mgray\x1b[0m \x1b[38:2::10:200:30mcolon\x1b[0m \x1b[44mbg\x1b[0m",
];

fn screen() -> Vec<String> {
    SCREEN.iter().map(|line| line.to_string()).collect()
}

const DARK: TerminalDefaults = TerminalDefaults { foreground: [229.0, 229.0, 231.0], background: [0.0, 0.0, 0.0] };

fn check_frames(name: &str, width: usize, height: usize, model_for: bool, colors: TerminalDefaults, times: &[f64]) {
    let _theme = global_state();
    let _kb = setup();
    dark_truecolor();
    let (_terminal, tui) = test_tui(width, height);
    let model =
        if model_for { pi_logo_model(ModelOrigin { column: 0, row: 0 }) } else { armin_model([138.0, 190.0, 183.0]) };
    let (now, clock) = clock();
    let mut animation = EasterEgg3dAnimation::with_clock(&tui, screen(), model, colors, clock, || {});
    let expected = FIXTURES[name].as_array().expect("frames");
    let mut frame = 0;
    let mut compare = |animation: &mut EasterEgg3dAnimation, label: String| {
        let lines = animation.render(width);
        assert_eq!(trimmed(lines.clone()), trimmed(strings(&expected[frame])), "{name} plain at {label}");
        assert_eq!(lines, strings(&expected[frame]), "{name} at {label}");
        frame += 1;
    };
    for &time in times {
        now.set(1000.0 + time * 1000.0);
        compare(&mut animation, format!("t={time}"));
    }
    if name == "pi_logo" {
        now.set(1000.0 + 10.0 * 1000.0);
        animation.close();
        assert!(animation.is_closing());
        for exit in [0.2, 0.6, 1.0] {
            now.set(1000.0 + (10.0 + exit) * 1000.0);
            compare(&mut animation, format!("exit+{exit}"));
        }
    }
    assert_eq!(frame, expected.len());
}

#[test]
fn pi_logo_frames_match_upstream() {
    check_frames("pi_logo", 40, 14, true, DARK, &[0.0, 0.25, 0.5, 1.0, 1.6, 3.0, 4.6, 6.0, 9.5]);
}

#[test]
fn armin_3d_frames_match_upstream() {
    check_frames("armin_3d", 60, 24, false, DARK, &[0.5, 2.0, 5.0, 7.3]);
}

#[test]
fn light_background_halo_frames_match_upstream() {
    let light = TerminalDefaults { foreground: [30.0, 30.0, 30.0], background: [255.0, 255.0, 255.0] };
    check_frames("light", 40, 14, true, light, &[0.4, 3.0, 5.5]);
}

#[test]
fn hash_matches_upstream() {
    let inputs: [i64; 6] = [0, 1, 7919, -5, 123_456_789, 65_537 * 40 * 7919 + 131 * 20];
    let expected: Vec<f64> = FIXTURES["hash"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
    let actual: Vec<f64> = inputs.iter().map(|&value| hash(value)).collect();
    assert_eq!(actual, expected);
}

#[test]
fn shuffle_is_deterministic_and_never_stacks_blocks() {
    let model = armin_model([1.0, 2.0, 3.0]);
    let steps = shuffle_steps(&model, 3);
    assert_eq!(steps, shuffle_steps(&model, 3));
    assert_ne!(steps, shuffle_steps(&model, 4));
    assert_eq!(steps.len(), 13);
    for (step, positions) in steps.iter().enumerate() {
        let mut seen = std::collections::HashSet::new();
        assert!(positions.iter().all(|position| seen.insert(*position)), "step {step}");
        assert!(positions.iter().all(|[x, y, z]| (0..31).contains(x) && (0..36).contains(y) && (-1..=1).contains(z)));
    }
    for pair in steps.windows(2) {
        for (from, to) in pair[0].iter().zip(&pair[1]) {
            let distance: i64 = (0..3).map(|axis| (from[axis] - to[axis]).abs()).sum();
            assert!(distance <= 1);
        }
    }
    let logo = pi_logo_model(ModelOrigin { column: 2, row: 1 });
    assert_eq!(logo.blocks.len(), 10);
    let moved =
        |steps: &[Vec<[i64; 3]>], step: usize| steps[step].iter().zip(&steps[step + 1]).filter(|(a, b)| a != b).count();
    let logo_steps = shuffle_steps(&logo, 0);
    assert!((0..12).all(|step| (1..=2).contains(&moved(&logo_steps, step))));
}

#[test]
fn parse_screen_resolves_colors_and_wide_graphemes() {
    let _theme = global_state();
    let lines = parse_screen(&screen(), 40, [200.0; 3], [0.0; 3]);
    let first = &lines[0];
    assert_eq!(first[0].fg, [228.0, 138.0, 122.0]);
    assert_eq!(first[6].fg, [200.0; 3]);
    let dim = lines[1].iter().find(|cell| cell.text == "i").unwrap();
    assert_eq!(dim.fg, [120.0; 3]);
    let panel = &lines[2];
    assert_eq!(panel[0].bg, Some([40.0, 40.0, 60.0]));
    let wide = panel.iter().position(|cell| cell.text == "日").unwrap();
    assert_eq!((panel[wide].width, panel[wide + 1].width, panel[wide + 1].text.as_str()), (2, 0, ""));
    let inverse = &lines[3][0];
    assert_eq!((inverse.fg, inverse.bg), ([0.0; 3], Some([200.0; 3])));
    let text: String = lines[3].iter().map(|cell| cell.text.as_str()).collect();
    assert_eq!(text, "inverse red link ─── ...");
    assert_eq!(lines[6][5].fg, [10.0, 200.0, 30.0]);
    let clipped = parse_screen(&screen(), 5, [200.0; 3], [0.0; 3]);
    assert!(clipped.iter().all(|line| line.len() <= 5));
    assert_eq!((glyph_ink(" ", 0.5), glyph_ink(".", 0.5), glyph_ink("─", 0.5), glyph_ink("a", 0.5)), (0, 2, 3, 5));
}

fn mouse(kind: TuiMouseEventType) -> TuiMouseEvent {
    TuiMouseEvent {
        kind,
        button: TuiMouseButton::Left,
        x: 3,
        y: 3,
        screen_x: 3,
        screen_y: 3,
        width: 40,
        height: 14,
        shift: false,
        alt: false,
        ctrl: false,
        wheel_delta: None,
        click_count: Some(1),
    }
}

#[test]
fn escape_plays_the_exit_and_a_second_escape_skips_it() {
    let _theme = global_state();
    let done = Calls::new();
    let (now, clock) = clock();
    let counter = RefCell::new(Some(done.counter()));
    let harness = Harness::new(40, 14, move |tui| {
        let on_done = counter.borrow_mut().take().unwrap();
        let model = pi_logo_model(ModelOrigin { column: 0, row: 0 });
        EasterEgg3dAnimation::with_clock(tui, screen(), model, DARK, clock, on_done)
    });
    now.set(4000.0);
    harness.send(keys::ESCAPE);
    assert!(harness.component.borrow().is_closing());
    assert!(done.is_empty());
    harness.send(keys::ESCAPE);
    assert_eq!(done.len(), 1);
    assert!(!harness.component.borrow().is_running());
}

#[test]
fn ctrl_c_and_clicks_close_and_the_timer_finishes_the_exit() {
    let _theme = global_state();
    let done = Calls::new();
    let (now, clock) = clock();
    let counter = RefCell::new(Some(done.counter()));
    let harness = Harness::new(40, 14, move |tui| {
        let on_done = counter.borrow_mut().take().unwrap();
        let model = pi_logo_model(ModelOrigin { column: 0, row: 0 });
        EasterEgg3dAnimation::with_clock(tui, screen(), model, DARK, clock, on_done)
    });
    harness.send(&keys::ctrl('c'));
    assert!(harness.component.borrow().is_closing());
    now.set(1000.0 + 500.0);
    harness.run_for(Duration::from_millis(60));
    assert!(done.is_empty());
    now.set(1000.0 + 1200.0);
    harness.run_for(Duration::from_millis(60));
    assert_eq!(done.len(), 1);

    let mut animation =
        EasterEgg3dAnimation::with_clock(&harness.tui, screen(), armin_model([9.0; 3]), DARK, Rc::new(|| 0.0), || {});
    let response = animation.handle_mouse(&mouse(TuiMouseEventType::Move));
    assert!(matches!(response, Some(MouseResponse::Handled(result)) if result.handled && result.render == Some(false)));
    assert!(!animation.is_closing());
    animation.handle_mouse(&mouse(TuiMouseEventType::Click));
    assert!(animation.is_closing());
}

#[test]
fn the_timer_finishes_when_rendering_stops() {
    let _theme = global_state();
    let (_terminal, tui) = test_tui(40, 14);
    let done = Calls::new();
    let (now, clock) = clock();
    let animation =
        EasterEgg3dAnimation::with_clock(&tui, screen(), armin_model([9.0; 3]), DARK, clock, done.counter());
    now.set(1900.0);
    ui_loop::run_for(Duration::from_millis(60)).unwrap();
    assert!(done.is_empty() && animation.is_running());
    now.set(2100.0);
    ui_loop::run_for(Duration::from_millis(60)).unwrap();
    assert_eq!(done.len(), 1);
    assert!(!animation.is_running());
    let mut other =
        EasterEgg3dAnimation::with_clock(&tui, screen(), armin_model([9.0; 3]), DARK, Rc::new(|| 0.0), || {});
    other.disposer()();
    assert!(!other.is_running());
    other.dispose();
}

#[test]
fn play_needs_fullscreen_and_shows_an_overlay_until_dismissed() {
    let _theme = global_state();
    let _kb = setup();
    let (_terminal, main) = test_tui(40, 14);
    assert!(!play_armin_3d(&main));
    assert!(!main.has_overlay());

    let terminal = VirtualTerminal::shared(40, 14);
    let tui = Tui::new_alt_screen(terminal.clone(), TuiOptions::default(), TuiAltScreenOptions::default());
    tui.add_child(pi_tui::component(EarendilAnnouncementComponent::new()));
    tui.start();
    ui_loop::run_for(Duration::from_millis(30)).unwrap();
    play_pi_logo_3d(&tui, 0, 0);
    assert!(is_playing());
    assert!(play_easter_egg_3d(&tui, EasterEgg3d::Armin));
    ui_loop::run_for(Duration::from_millis(200)).unwrap();
    assert!(tui.has_overlay());
    assert!(play_armin_3d(&tui), "an overlay is up, so nothing else starts");
    terminal.borrow().send_input(keys::ESCAPE);
    ui_loop::run_for(Duration::from_millis(30)).unwrap();
    terminal.borrow().send_input(keys::ESCAPE);
    ui_loop::run_for(Duration::from_millis(30)).unwrap();
    assert!(!tui.has_overlay());
    assert!(!is_playing());
    tui.stop(Default::default());
}

#[test]
fn earendil_announcement_shows_title_link_and_image() {
    let _theme = global_state();
    let mut announcement = EarendilAnnouncementComponent::new();
    let lines = trimmed(announcement.render(70));
    assert!(lines[0].starts_with("──────"), "{lines:?}");
    assert_eq!(lines[1], " pi has joined Earendil");
    assert_eq!(lines[2], "");
    assert_eq!(lines[3], " Read the blog post:");
    assert_eq!(lines[4], format!(" {BLOG_URL}"));
    assert!(lines.len() > 7, "the image or its fallback follows: {lines:?}");
    assert!(lines.last().unwrap().starts_with("──────"));
}
