//! Port of upstream `test/bug-regression-isimageline-startswith-bug.test.ts`.
//! An older `is_image_line` only checked the line start, so lines with text before an image
//! sequence skipped the image path and failed the width check.

use pi_tui::terminal_image::is_image_line;

#[test]
fn old_implementation_returned_false_for_lines_with_image_sequences() {
    let old_is_image_line = |line: &str, prefix: Option<&str>| prefix.is_some_and(|prefix| line.starts_with(prefix));
    let line = "Read image file [image/jpeg]\x1b]1337;File=size=800,600;inline=1:base64data...\x07";
    assert!(!old_is_image_line(line, None));
}

#[test]
fn new_implementation_returns_true_for_text_before_the_sequence() {
    assert!(is_image_line("Read image file [image/jpeg]\x1b]1337;File=size=800,600;inline=1:base64data...\x07"));
}

#[test]
fn detects_kitty_sequences_in_any_position() {
    let long_line = format!("Text before \x1b_Ga=T,f=100{} text after", "A".repeat(300_000));
    let scenarios = [
        "At start: \x1b_Ga=T,f=100,data...\x1b\\",
        "Prefix \x1b_Ga=T,data...\x1b\\",
        "Suffix text \x1b_Ga=T,data...\x1b\\ suffix",
        "Middle \x1b_Ga=T,data...\x1b\\ more text",
        long_line.as_str(),
    ];
    for line in scenarios {
        assert!(is_image_line(line), "should detect Kitty sequence in: {:?}", &line[..50.min(line.len())]);
    }
}

#[test]
fn detects_iterm2_sequences_in_any_position() {
    let long_line = format!("Text before \x1b]1337;File=size=800,600;inline=1:{} text after", "B".repeat(300_000));
    let scenarios = [
        "At start: \x1b]1337;File=size=100,100:base64...\x07",
        "Prefix \x1b]1337;File=inline=1:data==\x07",
        "Suffix text \x1b]1337;File=inline=1:data==\x07 suffix",
        "Middle \x1b]1337;File=inline=1:data==\x07 more text",
        long_line.as_str(),
    ];
    for line in scenarios {
        assert!(is_image_line(line), "should detect iTerm2 sequence in: {:?}", &line[..50.min(line.len())]);
    }
}

#[test]
fn detects_image_sequences_in_read_tool_output() {
    assert!(is_image_line("Read image file [image/jpeg]\x1b]1337;File=size=800,600;inline=1:base64image...\x07"));
}

#[test]
fn detects_kitty_sequences_from_the_image_component() {
    assert!(is_image_line("\x1b_Ga=T,f=100,t=f,d=base64data...\x1b\\\x1b_Gm=i=1;\x1b\\"));
}

#[test]
fn handles_ansi_codes_before_image_sequences() {
    for line in [
        "\x1b[31mError\x1b[0m: \x1b]1337;File=inline=1:base64==\x07",
        "\x1b[33mWarning\x1b[0m: \x1b_Ga=T,data...\x1b\\",
        "\x1b[1mBold\x1b[0m \x1b]1337;File=:base64==\x07\x1b[0m",
    ] {
        assert!(is_image_line(line), "should detect image sequence after ANSI codes: {line:?}");
    }
}

#[test]
fn does_not_crash_on_very_long_lines_with_image_sequences() {
    let crash_line =
        format!("Output: \x1b]1337;File=size=800,600;inline=1:{} end of output", "A".repeat(100).repeat(3040));
    assert!(crash_line.len() > 300_000);
    assert!(is_image_line(&crash_line));
}

#[test]
fn handles_lines_exactly_matching_crash_log_dimensions() {
    let target_width = 58_649;
    let (prefix, sequence, suffix) = ("Text", "\x1b_Ga=T,f=100", "End");
    let padding = "A".repeat(target_width - prefix.len() - sequence.len() - suffix.len());
    let line = format!("{prefix}{sequence}{padding}{suffix}");
    assert_eq!(line.len(), 58_649);
    assert!(is_image_line(&line));
}

#[test]
fn does_not_detect_images_in_regular_long_text() {
    assert!(!is_image_line(&"A".repeat(100_000)));
}

#[test]
fn does_not_detect_images_in_lines_with_file_paths() {
    for path in [
        "/path/to/1337/image.jpg",
        "/usr/local/bin/File_converter",
        "~/Documents/1337File_backup.png",
        "./_G_test_file.txt",
    ] {
        assert!(!is_image_line(path), "should not detect an image sequence in path: {path}");
    }
}
