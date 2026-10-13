//! Hot reload of the active user theme.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pi_tui::colors::color_to_hex;
use serde_json::json;

use super::*;

fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn reloads_the_active_user_theme_when_its_file_changes() {
    let _state = global_state();
    let dir = tempfile::tempdir().expect("temp dir");
    let mut json = builtin_json("dark");
    json["name"] = json!("live");
    json["colors"]["accent"] = json!("#112233");
    let path = write_theme(dir.path(), &json);
    set_custom_themes_dir(Some(dir.path().to_path_buf()));
    let changes = Arc::new(AtomicUsize::new(0));
    let counter = changes.clone();
    on_theme_change(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });

    init_theme(Some("live"), true);
    assert_eq!(color_to_hex(&theme().color(ThemeColor::Accent)), "#112233");

    // An invalid intermediate state keeps the last good theme.
    std::fs::write(&path, "{ \"name\": ").expect("write partial");
    json["colors"]["accent"] = json!("#445566");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(color_to_hex(&theme().color(ThemeColor::Accent)), "#112233");
    std::fs::write(&path, json.to_string()).expect("write theme");
    let reloaded = wait_until(|| color_to_hex(&theme().color(ThemeColor::Accent)) == "#445566");
    assert!(reloaded, "theme was not reloaded");
    assert!(changes.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        get_theme_by_name("live").map(|theme| color_to_hex(&theme.color(ThemeColor::Accent))).as_deref(),
        Some("#445566")
    );

    // Switching to a built-in theme stops watching the file.
    set_theme("dark", true).expect("dark");
    json["colors"]["accent"] = json!("#778899");
    std::fs::write(&path, json.to_string()).expect("write theme");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(theme().name(), Some("dark"));
    on_theme_change(|| {});
}
