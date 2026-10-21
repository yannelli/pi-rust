//! Port of the `isImageLine` and `detectCapabilities` cases in upstream `test/terminal-image.test.ts`.

use std::cell::Cell;

use pi_tui::env::{MapEnv, global_state_lock};
use pi_tui::terminal_image::{
    CapabilityOverrides, ImageProtocol, TerminalCapabilities, detect_capabilities_with, get_capabilities_with,
    is_image_line, probe_tmux_hyperlinks, reset_capabilities_cache, set_capability_overrides,
};

fn env(pairs: &[(&str, &str)]) -> MapEnv {
    MapEnv::new(pairs.iter().copied())
}

/// Detects with a tmux probe that must not run, matching upstream calls that pass no probe outside tmux.
fn detect(pairs: &[(&str, &str)]) -> TerminalCapabilities {
    detect_capabilities_with(&env(pairs), &|| panic!("tmux probe ran outside tmux"))
}

fn detect_with_probe(pairs: &[(&str, &str)], forwards: bool) -> TerminalCapabilities {
    detect_capabilities_with(&env(pairs), &move || forwards)
}

fn caps(images: Option<ImageProtocol>, true_color: bool, hyperlinks: bool) -> TerminalCapabilities {
    TerminalCapabilities { images, true_color, hyperlinks }
}

const TMUX: &str = "/tmp/tmux-1000/default,1234,0";

#[test]
fn detects_iterm2_sequences() {
    assert!(is_image_line("\x1b]1337;File=size=100,100;inline=1:base64encodeddata==\x07"));
    assert!(is_image_line("Some text \x1b]1337;File=size=100,100;inline=1:base64data==\x07 more text"));
    assert!(is_image_line("Text before image...\x1b]1337;File=inline=1:verylongbase64data==...text after"));
    assert!(is_image_line("Regular text ending with \x1b]1337;File=inline=1:base64data==\x07"));
    assert!(is_image_line("\x1b]1337;File=:\x07"));
}

#[test]
fn detects_kitty_sequences() {
    assert!(is_image_line("\x1b_Ga=T,f=100,t=f,d=base64data...\x1b\\\x1b_Gm=i=1;\x1b\\"));
    assert!(is_image_line("Output: \x1b_Ga=T,f=100;data...\x1b\\\x1b_Gm=i=1;\x1b\\"));
    assert!(is_image_line("  \x1b_Ga=T,f=100...\x1b\\\x1b_Gm=i=1;\x1b\\  "));
}

#[test]
fn detects_image_sequences_in_bug_regression_scenarios() {
    let long_line = format!("Text prefix \x1b]1337;File=size=800,600;inline=1:{} suffix", "A".repeat(100).repeat(3000));
    assert!(long_line.len() > 300_000);
    assert!(is_image_line(&long_line));
    assert!(is_image_line("Read image file [image/jpeg]\x1b]1337;File=inline=1:base64data==\x07"));
    assert!(is_image_line("\x1b[31mError output \x1b]1337;File=inline=1:image==\x07"));
    assert!(is_image_line("\x1b_Ga=T,f=100:data...\x1b\\\x1b_Gm=i=1;\x1b\\\x1b[0m reset"));
}

#[test]
fn does_not_detect_images_in_lines_without_images() {
    assert!(!is_image_line("This is just a regular text line without any escape sequences"));
    assert!(!is_image_line("\x1b[31mRed text\x1b[0m and \x1b[32mgreen text\x1b[0m"));
    assert!(!is_image_line("\x1b[1A\x1b[2KLine cleared and moved up"));
    assert!(!is_image_line("Some text with ]1337;File but missing ESC at start"));
    assert!(!is_image_line("Some text with _G but missing ESC at start"));
    assert!(!is_image_line(""));
    assert!(!is_image_line("\n"));
    assert!(!is_image_line("\n\n"));
}

#[test]
fn handles_mixed_content() {
    assert!(is_image_line("Kitty: \x1b_Ga=T...\x1b\\\x1b_Gm=i=1;\x1b\\ iTerm2: \x1b]1337;File=inline=1:data==\x07"));
    assert!(is_image_line("Start \x1b]1337;File=img1==\x07 middle \x1b]1337;File=img2==\x07 end"));
    assert!(!is_image_line("/path/to/File_1337_backup/image.jpg"));
}

#[test]
fn defaults_to_no_hyperlinks_for_unknown_terminals() {
    let detected = detect(&[]);
    assert!(!detected.hyperlinks);
    assert_eq!(detected.images, None);
}

#[test]
fn applies_environment_overrides() {
    let overridden = detect(&[("PI_HYPERLINKS", "1"), ("PI_IMAGE_PROTOCOL", "kitty"), ("PI_TRUE_COLOR", "1")]);
    assert_eq!(overridden, caps(Some(ImageProtocol::Kitty), true, true));
    let disabled = detect(&[
        ("TERM_PROGRAM", "iterm.app"),
        ("PI_HYPERLINKS", "0"),
        ("PI_IMAGE_PROTOCOL", "none"),
        ("PI_TRUE_COLOR", "0"),
    ]);
    assert_eq!(disabled, caps(None, false, false));
}

#[test]
fn preserves_auto_detection_for_auto_environment_overrides() {
    let detected = detect(&[
        ("TERM_PROGRAM", "ghostty"),
        ("PI_HYPERLINKS", "auto"),
        ("PI_IMAGE_PROTOCOL", "auto"),
        ("PI_TRUE_COLOR", "auto"),
    ]);
    assert_eq!(detected, caps(Some(ImageProtocol::Kitty), true, true));
}

#[test]
fn applies_and_clears_programmatic_overrides() {
    let _lock = global_state_lock();
    let environment = env(&[("PI_HYPERLINKS", "1"), ("PI_IMAGE_PROTOCOL", "kitty"), ("PI_TRUE_COLOR", "1")]);
    reset_capabilities_cache();
    set_capability_overrides(CapabilityOverrides {
        images: Some(None),
        true_color: Some(false),
        hyperlinks: Some(false),
    });
    let overridden = get_capabilities_with(&environment);
    set_capability_overrides(CapabilityOverrides::default());
    let cleared = get_capabilities_with(&environment);
    set_capability_overrides(CapabilityOverrides::default());
    reset_capabilities_cache();
    assert_eq!(overridden, caps(None, false, false));
    assert_eq!(cleared, caps(Some(ImageProtocol::Kitty), true, true));
}

#[test]
fn bypasses_the_tmux_probe_when_hyperlinks_are_overridden() {
    let probed = Cell::new(false);
    let environment = env(&[("TMUX", TMUX), ("PI_HYPERLINKS", "1"), ("PI_IMAGE_PROTOCOL", "kitty")]);
    let detected = detect_capabilities_with(&environment, &|| {
        probed.set(true);
        false
    });
    assert!(!probed.get());
    assert!(detected.hyperlinks);
    assert_eq!(detected.images, Some(ImageProtocol::Kitty));
}

#[test]
fn follows_the_tmux_probe_for_hyperlinks() {
    let forwarded = detect_with_probe(&[("TMUX", TMUX), ("TERM_PROGRAM", "ghostty")], true);
    assert!(forwarded.hyperlinks);
    assert_eq!(forwarded.images, None);
    let stripped = detect_with_probe(&[("TMUX", TMUX), ("TERM_PROGRAM", "ghostty")], false);
    assert!(!stripped.hyperlinks);
    assert_eq!(stripped.images, None);
}

#[test]
fn checks_tmux_capability_when_term_starts_with_tmux() {
    let pairs = [("TERM", "tmux-256color"), ("TERM_PROGRAM", "iterm.app")];
    let forwarded = detect_with_probe(&pairs, true);
    assert!(forwarded.hyperlinks);
    assert_eq!(forwarded.images, None);
    assert!(!detect_with_probe(&pairs, false).hyperlinks);
}

#[test]
fn forces_hyperlinks_off_when_term_starts_with_screen() {
    let detected = detect(&[("TERM", "screen-256color")]);
    assert!(!detected.hyperlinks);
    assert_eq!(detected.images, None);
}

#[test]
fn enables_hyperlinks_without_images_for_herdr() {
    let detected = detect(&[
        ("TERM_PROGRAM", "herdr"),
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("KITTY_WINDOW_ID", "1"),
    ]);
    assert_eq!(detected, caps(None, true, true));
    assert!(!detect(&[("TERM_PROGRAM", "herdr"), ("PI_HYPERLINKS", "0")]).hyperlinks);
}

#[test]
fn enables_hyperlinks_and_kitty_images_for_rex() {
    let detected = detect(&[("TERM_PROGRAM", "rex"), ("TERM", "xterm-256color"), ("KITTY_WINDOW_ID", "1")]);
    assert_eq!(detected, caps(Some(ImageProtocol::Kitty), true, true));
    assert!(!detect(&[("TERM_PROGRAM", "rex"), ("PI_HYPERLINKS", "0")]).hyperlinks);
}

#[test]
fn enables_kitty_images_and_hyperlinks_for_kitty_compatible_terminals() {
    let ghostty = detect(&[("TERM_PROGRAM", "ghostty")]);
    assert!(ghostty.hyperlinks);
    let cmux = detect(&[("TERM_PROGRAM", "ghostty"), ("CMUX_WORKSPACE_ID", "workspace")]);
    assert_eq!(cmux.images, Some(ImageProtocol::Kitty));
    assert!(cmux.hyperlinks);
    assert!(detect(&[("KITTY_WINDOW_ID", "1")]).hyperlinks);
    assert!(detect(&[("WEZTERM_PANE", "0")]).hyperlinks);
    assert_eq!(detect(&[("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty")]).images, Some(ImageProtocol::Kitty));
    assert_eq!(detect(&[("TERM", "xterm-ghostty")]).images, Some(ImageProtocol::Kitty));
}

#[test]
fn enables_images_and_hyperlinks_for_warp() {
    let kitty = caps(Some(ImageProtocol::Kitty), true, true);
    assert_eq!(detect(&[("TERM_PROGRAM", "WarpTerminal")]), kitty);
    assert_eq!(detect(&[("WARP_SESSION_ID", "some-session-id")]), kitty);
    assert_eq!(detect(&[("WARP_TERMINAL_SESSION_UUID", "d0e1a2e5-7ca7-44cd-9037-ac7222011161")]), kitty);
}

#[test]
fn disables_images_for_warp_inside_tmux() {
    let detected =
        detect_with_probe(&[("TERM_PROGRAM", "WarpTerminal"), ("TMUX", TMUX), ("TERM", "tmux-256color")], true);
    assert_eq!(detected.images, None);
    assert!(detected.hyperlinks);
}

#[test]
fn enables_hyperlinks_for_iterm2_vscode_and_zed() {
    let iterm = detect(&[("TERM_PROGRAM", "iterm.app")]);
    assert!(iterm.hyperlinks);
    assert_eq!(iterm.images, Some(ImageProtocol::Iterm2));
    assert_eq!(detect(&[("ITERM_SESSION_ID", "w0t0p0")]).images, Some(ImageProtocol::Iterm2));
    assert!(detect(&[("TERM_PROGRAM", "vscode")]).hyperlinks);
    assert_eq!(detect(&[("TERM_PROGRAM", "zed")]), caps(None, true, true));
    assert_eq!(detect(&[("TERM_PROGRAM", "Alacritty")]), caps(None, true, true));
}

#[test]
fn enables_truecolor_and_hyperlinks_for_windows_terminal_outside_multiplexers() {
    let detected = detect(&[("WT_SESSION", "session"), ("TERM", "xterm-256color")]);
    assert_eq!(detected, caps(None, true, true));
}

#[test]
fn enables_truecolor_without_hyperlinks_for_jetbrains_terminal() {
    let detected = detect(&[("TERMINAL_EMULATOR", "JetBrains-JediTerm"), ("TERM", "xterm-256color")]);
    assert_eq!(detected, caps(None, true, false));
}

#[test]
fn does_not_inherit_windows_terminal_truecolor_through_tmux() {
    let detected = detect_with_probe(&[("WT_SESSION", "session"), ("TMUX", TMUX), ("TERM", "tmux-256color")], false);
    assert_eq!(detected, caps(None, false, false));
}

#[test]
fn trusts_explicit_truecolor_hints_through_tmux() {
    let detected = detect_with_probe(&[("COLORTERM", "truecolor"), ("TMUX", TMUX), ("TERM", "tmux-256color")], false);
    assert_eq!(detected, caps(None, true, false));
}

#[test]
fn detects_truecolor_from_direct_color_term_values() {
    assert!(detect_with_probe(&[("TERM", "xterm-direct")], false).true_color);
    assert!(detect(&[("COLORTERM", "24bit")]).true_color);
}

#[test]
fn ignores_empty_variables_like_upstream() {
    let detected = detect(&[("TMUX", ""), ("KITTY_WINDOW_ID", ""), ("PI_IMAGE_PROTOCOL", "ITERM2")]);
    assert_eq!(detected.images, Some(ImageProtocol::Iterm2));
    assert_eq!(detect(&[("PI_IMAGE_PROTOCOL", "0"), ("KITTY_WINDOW_ID", "1")]).images, None);
}

#[test]
fn tmux_probe_answers_within_its_timeout() {
    let started = std::time::Instant::now();
    let _ = probe_tmux_hyperlinks();
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}
