//! Port of the portable cases in upstream `test/native-platform.test.ts`.

use pi_tui::native_platform::{ClipboardContent, ModifierKey};

#[test]
fn names_modifier_keys_like_upstream() {
    let names: Vec<&str> = [ModifierKey::Shift, ModifierKey::Command, ModifierKey::Control, ModifierKey::Option]
        .map(ModifierKey::as_str)
        .into();
    assert_eq!(names, ["shift", "command", "control", "option"]);
}

#[test]
fn separates_unavailable_from_empty_reads() {
    assert!(ClipboardContent::<String>::Unavailable.is_unavailable());
    assert!(!ClipboardContent::<String>::Empty.is_unavailable());
    assert_eq!(ClipboardContent::Data("x".to_owned()).data().as_deref(), Some("x"));
    assert_eq!(ClipboardContent::<String>::Empty.data(), None);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_opens_x11_lazily_and_rechecks_display() {
    use std::sync::Arc;

    use pi_tui::env::MapEnv;
    use pi_tui::native_platform::{get_native_clipboard_with_env, is_native_modifier_pressed};

    // Wayland paste uses wl-paste, so only DISPLAY enables the native clipboard.
    let wayland = MapEnv::new([("WAYLAND_DISPLAY", "wayland-0")]);
    assert!(get_native_clipboard_with_env(&wayland).is_none());
    let clipboard = get_native_clipboard_with_env(&MapEnv::new([("DISPLAY", ":0")])).expect("clipboard with DISPLAY");
    assert!(!clipboard.supports_set_text());
    assert_eq!(clipboard.get_file_paths(), Ok(ClipboardContent::Unavailable));
    if std::env::var_os("DISPLAY").is_none() {
        // No X server is reachable, which reads report as unavailable rather than as errors.
        assert_eq!(clipboard.get_text(), Ok(ClipboardContent::Unavailable));
        assert_eq!(clipboard.get_image(), Ok(ClipboardContent::Unavailable));
    }
    assert!(get_native_clipboard_with_env(&MapEnv::new([("DISPLAY", "")])).is_none());
    let again = get_native_clipboard_with_env(&MapEnv::new([("DISPLAY", ":1")])).expect("clipboard with DISPLAY");
    assert!(Arc::ptr_eq(&clipboard, &again));
    assert!(!is_native_modifier_pressed(ModifierKey::Shift));
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn uses_the_native_platform_helper_directly_as_the_clipboard_api() {
    use std::sync::Arc;

    use pi_tui::native_platform::get_native_clipboard;

    let clipboard = get_native_clipboard().expect("native clipboard");
    assert!(clipboard.supports_set_text());
    let again = get_native_clipboard().expect("native clipboard");
    assert!(Arc::ptr_eq(&clipboard, &again));
}

/// Opt in on a Windows test desktop: this replaces the system clipboard contents.
#[cfg(windows)]
#[test]
fn writes_windows_clipboard_text_through_the_native_helper() {
    use pi_tui::native_platform::get_native_clipboard;

    if std::env::var("PI_TEST_NATIVE_CLIPBOARD").as_deref() != Ok("1") {
        return;
    }
    let clipboard = get_native_clipboard().expect("native clipboard");
    for text in ["clipboard café 日本語", "", "second write"] {
        clipboard.set_text(text).expect("set text");
        let read = clipboard.get_text().expect("get text");
        assert!(
            read == ClipboardContent::Data(text.to_owned()) || (text.is_empty() && read == ClipboardContent::Empty)
        );
        assert_eq!(clipboard.get_image(), Ok(ClipboardContent::Empty));
    }
}
