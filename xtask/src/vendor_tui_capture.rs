//! Apply upstream's focused viewport-key capture to older bundled TUI copies without refreshing other behavior.

use std::collections::BTreeMap;

const FIELD: &str = "\tcapturesViewportInput?: boolean;";
const READ_CAPTURE: &str = "\t\tif (this.getFocusedComponent()?.capturesViewportInput) return true;";
const OLD_HELPER: &str = "\tprivate shouldDeferViewportInputToOverlay(): boolean {\n\t\treturn this.isOverlayFocused() && this.activeSearch?.overlay?.isFocused() !== true;\n\t}";
const KEYBOARD_OLD: &str = "\t\tif (this.shouldDeferViewportInputToOverlay()) return undefined;\n\t\tif (keybindings.matches(data, \"tui.altScreen.pageUp\")) {";
const KEYBOARD_NEW: &str = "\t\tif (this.shouldDeferViewportInput()) return undefined;\n\t\tif (keybindings.matches(data, \"tui.altScreen.pageUp\")) {";

fn replace_once(source: &mut String, before: &str, after: &str, key: &str) -> Result<(), String> {
    if source.matches(before).count() != 1 {
        return Err(format!("{key}: expected one viewport capture patch site"));
    }
    *source = source.replacen(before, after, 1);
    Ok(())
}

pub(crate) fn apply(files: &mut BTreeMap<String, String>) -> Result<(), String> {
    let key = "pi-tui/src/tui.ts";
    let source = files.get_mut(key).ok_or_else(|| format!("missing {key}"))?;
    if !source.contains(FIELD) {
        let before = "\twantsKeyRelease?: boolean;";
        let after = format!(
            "{before}\n\n\t/** Receives fullscreen viewport keys while focused, instead of scrolling the transcript. */\n{FIELD}"
        );
        replace_once(source, before, &after, key)?;
    }

    let key = "pi-tui/src/tui-alt-screen.ts";
    let source = files.get_mut(key).ok_or_else(|| format!("missing {key}"))?;
    if source.contains("private shouldDeferViewportInput(): boolean {") {
        if !source.contains(READ_CAPTURE) || !source.contains(KEYBOARD_NEW) {
            return Err(format!("{key}: incomplete viewport capture handler"));
        }
        return Ok(());
    }
    let helper = format!(
        "\t/** Focused capturing components receive viewport keys instead of scrolling the transcript. */\n\tprivate shouldDeferViewportInput(): boolean {{\n{READ_CAPTURE}\n\t\treturn this.shouldDeferViewportInputToOverlay();\n\t}}\n\n{OLD_HELPER}"
    );
    replace_once(source, OLD_HELPER, &helper, key)?;
    replace_once(source, KEYBOARD_OLD, KEYBOARD_NEW, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(viewport: String) -> BTreeMap<String, String> {
        [
            ("pi-tui/src/tui.ts".into(), "interface Component {\n\twantsKeyRelease?: boolean;\n}".into()),
            ("pi-tui/src/tui-alt-screen.ts".into(), viewport),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn captures_keyboard_input_without_changing_the_existing_wheel_rule() {
        let wheel = "\t\t\tif (this.shouldDeferViewportInputToOverlay()) return undefined;";
        let mut files = files(format!("{OLD_HELPER}\n{wheel}\n{KEYBOARD_OLD}"));
        apply(&mut files).unwrap();
        assert!(files["pi-tui/src/tui.ts"].contains(FIELD));
        let viewport = &files["pi-tui/src/tui-alt-screen.ts"];
        assert!(viewport.contains(READ_CAPTURE));
        assert!(viewport.contains(KEYBOARD_NEW));
        assert!(viewport.contains(wheel));
        assert!(viewport.contains(OLD_HELPER));
        let patched = files.clone();
        apply(&mut files).unwrap();
        assert_eq!(files, patched);
    }

    #[test]
    fn leaves_the_upstream_capture_handler_unchanged() {
        let viewport = format!(
            "\tprivate shouldDeferViewportInput(): boolean {{\n{READ_CAPTURE}\n\t\treturn this.isOverlayFocused() && this.activeSearch?.overlay?.isFocused() !== true;\n\t}}\n{KEYBOARD_NEW}"
        );
        let mut files = files(viewport);
        files.get_mut("pi-tui/src/tui.ts").unwrap().push_str(FIELD);
        let original = files.clone();
        apply(&mut files).unwrap();
        assert_eq!(files, original);
    }

    #[test]
    fn rejects_an_unrecognized_viewport_handler() {
        let mut files = files("private shouldDeferViewportInputToOverlay() {}".into());
        assert_eq!(
            apply(&mut files).unwrap_err(),
            "pi-tui/src/tui-alt-screen.ts: expected one viewport capture patch site"
        );
    }
}
