//! WSL and Termux detection and the key defaults that differ on Windows, WSL, and macOS
//! (upstream `utils/wsl.ts`, `core/keybindings.ts`, `docs/termux.md`).

use crate::clipboard::{ClipboardEnv, Platform};

/// Platform, environment variables, and `/proc/version`, shared with the clipboard code.
pub type PlatformEnv = ClipboardEnv;

/// Windows Subsystem for Linux, where Windows programs are reachable through interop.
pub fn is_wsl(env: &PlatformEnv) -> bool {
    crate::clipboard::is_wsl(env)
}

/// Termux on Android: an Android build, `TERMUX_VERSION`, or a Termux `PREFIX`.
pub fn is_termux(env: &PlatformEnv) -> bool {
    env.platform == Platform::Android
        || env.var("TERMUX_VERSION").is_some()
        || env.var("PREFIX").is_some_and(|prefix| prefix.contains("/com.termux/"))
}

/// Termux package that provides a helper tool; Linux downloads do not run on Android.
pub fn termux_package_for_tool(tool: &str) -> &str {
    match tool {
        "rg" => "ripgrep",
        other => other,
    }
}

/// Windows and WSL terminals reserve some key combinations, so pi uses other defaults there.
pub fn use_windows_keybindings(env: &PlatformEnv) -> bool {
    env.platform == Platform::Windows
        || (env.platform == Platform::Linux
            && (env.var("WSL_DISTRO_NAME").is_some() || env.var("WSL_INTEROP").is_some()))
}

/// Default keys of the actions whose defaults depend on the platform, by action id.
pub fn app_key_defaults(env: &PlatformEnv) -> Vec<(&'static str, Vec<&'static str>)> {
    let windows = env.platform == Platform::Windows;
    let windows_keys = use_windows_keybindings(env);
    let mac = env.platform == Platform::MacOs;
    let pick = |on: bool, yes: &[&'static str], no: &[&'static str]| if on { yes.to_vec() } else { no.to_vec() };
    vec![
        (
            "tui.editor.undo",
            vec![if windows {
                "ctrl+z"
            } else if windows_keys {
                "alt+z"
            } else {
                "ctrl+-"
            }],
        ),
        ("tui.altScreen.previousPrompt", pick(windows_keys, &["ctrl+up"], &["ctrl+shift+up", "ctrl+up"])),
        ("tui.altScreen.nextPrompt", pick(windows_keys, &["ctrl+down"], &["ctrl+shift+down", "ctrl+down"])),
        ("tui.altScreen.search", pick(windows_keys, &["ctrl+f"], &["ctrl+shift+f"])),
        ("app.suspend", pick(windows, &[], &["ctrl+z"])),
        ("app.model.cycleBackward", pick(windows_keys, &["alt+p"], &["shift+ctrl+p"])),
        ("app.message.followUp", pick(windows_keys, &["ctrl+q"], &["alt+enter"])),
        ("app.message.dequeue", pick(windows_keys, &["alt+q"], &["alt+up"])),
        ("app.clipboard.pasteImage", pick(windows_keys, &["alt+v"], &["ctrl+v"])),
        ("app.tree.foldOrUp", pick(mac, &["alt+left", "ctrl+left"], &["ctrl+left", "alt+left"])),
        ("app.tree.unfoldOrDown", pick(mac, &["alt+right", "ctrl+right"], &["ctrl+right", "alt+right"])),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(platform: Platform, vars: &[(&str, &str)], proc_version: Option<&str>) -> PlatformEnv {
        PlatformEnv {
            platform,
            vars: vars.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect(),
            proc_version: proc_version.map(String::from),
        }
    }

    fn defaults(env: &PlatformEnv, action: &str) -> Vec<&'static str> {
        app_key_defaults(env).into_iter().find(|(id, _)| *id == action).unwrap().1
    }

    #[test]
    fn wsl_comes_from_variables_or_the_kernel_release() {
        assert!(is_wsl(&env(Platform::Linux, &[("WSL_DISTRO_NAME", "Ubuntu")], None)));
        assert!(is_wsl(&env(Platform::Linux, &[("WSLENV", "PATH/l")], None)));
        let release = "Linux version 5.15.153.1-microsoft-standard-WSL2";
        assert!(is_wsl(&env(Platform::Linux, &[], Some(release))));
        assert!(!is_wsl(&env(Platform::Linux, &[("WSL_DISTRO_NAME", "")], Some("Linux version 6.8.0-generic"))));
    }

    #[test]
    fn termux_comes_from_android_or_termux_variables() {
        assert!(is_termux(&env(Platform::Android, &[], None)));
        assert!(is_termux(&env(Platform::Linux, &[("TERMUX_VERSION", "0.118.1")], None)));
        assert!(is_termux(&env(Platform::Linux, &[("PREFIX", "/data/data/com.termux/files/usr")], None)));
        assert!(!is_termux(&env(Platform::Linux, &[("PREFIX", "/usr/local")], None)));
        assert_eq!(termux_package_for_tool("rg"), "ripgrep");
        assert_eq!(termux_package_for_tool("fd"), "fd");
    }

    #[test]
    fn windows_and_wsl_use_their_own_key_defaults() {
        let linux = env(Platform::Linux, &[], None);
        let wsl = env(Platform::Linux, &[("WSL_INTEROP", "/run/WSL/1_interop")], None);
        let windows = env(Platform::Windows, &[], None);
        let mac = env(Platform::MacOs, &[], None);
        assert!(!use_windows_keybindings(&linux) && use_windows_keybindings(&wsl) && use_windows_keybindings(&windows));
        assert!(!use_windows_keybindings(&env(Platform::MacOs, &[("WSL_INTEROP", "x")], None)));

        assert_eq!(defaults(&linux, "tui.editor.undo"), ["ctrl+-"]);
        assert_eq!(defaults(&wsl, "tui.editor.undo"), ["alt+z"]);
        assert_eq!(defaults(&windows, "tui.editor.undo"), ["ctrl+z"]);
        assert!(defaults(&windows, "app.suspend").is_empty());
        assert_eq!(defaults(&wsl, "app.suspend"), ["ctrl+z"]);
        assert_eq!(defaults(&wsl, "app.message.followUp"), ["ctrl+q"]);
        assert_eq!(defaults(&linux, "app.clipboard.pasteImage"), ["ctrl+v"]);
        assert_eq!(defaults(&windows, "tui.altScreen.search"), ["ctrl+f"]);
        assert_eq!(defaults(&mac, "app.tree.foldOrUp"), ["alt+left", "ctrl+left"]);
        assert_eq!(defaults(&linux, "app.tree.foldOrUp"), ["ctrl+left", "alt+left"]);
    }
}
