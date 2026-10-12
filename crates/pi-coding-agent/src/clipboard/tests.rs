use std::cell::RefCell;
use std::sync::Mutex;
use std::time::Duration;

use super::*;

type Call = (String, Vec<String>, Option<String>);
type Respond = dyn Fn(&str, &[String]) -> Option<Vec<u8>> + Sync;

struct Mock {
    calls: Mutex<Vec<Call>>,
    respond: Box<Respond>,
}

impl Mock {
    fn new(respond: impl Fn(&str, &[String]) -> Option<Vec<u8>> + Sync + 'static) -> Self {
        Self { calls: Mutex::new(Vec::new()), respond: Box::new(respond) }
    }

    fn ok() -> Self {
        Self::new(|_, _| Some(Vec::new()))
    }

    fn failing() -> Self {
        Self::new(|_, _| None)
    }

    fn names(&self) -> Vec<String> {
        self.calls.lock().unwrap().iter().map(|(name, _, _)| name.clone()).collect()
    }

    fn call(&self, index: usize) -> Call {
        self.calls.lock().unwrap()[index].clone()
    }
}

impl ClipboardRunner for Mock {
    fn run(&self, command: &str, args: &[String], input: Option<&[u8]>, timeout: Duration) -> Option<Vec<u8>> {
        assert!(timeout <= Duration::from_secs(5));
        let input = input.map(|bytes| String::from_utf8(bytes.to_vec()).unwrap());
        self.calls.lock().unwrap().push((command.to_string(), args.to_vec(), input));
        (self.respond)(command, args)
    }
}

fn env(platform: Platform, vars: &[(&str, &str)]) -> ClipboardEnv {
    let vars = vars.iter().map(|(key, value)| ((*key).to_string(), (*value).to_string())).collect();
    ClipboardEnv { platform, vars, proc_version: None }
}

/// Run `f` with a context and return the OSC 52 sequences it wrote.
fn with<T>(env: ClipboardEnv, runner: &Mock, f: impl FnOnce(&ClipboardContext<'_>) -> T) -> (T, Vec<String>) {
    let written = RefCell::new(Vec::new());
    let write = |sequence: &str| written.borrow_mut().push(sequence.to_string());
    let result = f(&ClipboardContext { env, runner, write_terminal: &write });
    (result, written.into_inner())
}

fn copy(env: ClipboardEnv, runner: &Mock, text: &str) -> (anyhow::Result<()>, Vec<String>) {
    with(env, runner, |context| copy_to_clipboard_with(context, text))
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_string()).collect()
}

#[test]
fn read_text_uses_platform_commands_and_stops_on_success() {
    for (var, command, expected_args, calls) in [
        ("WAYLAND_DISPLAY", "wl-paste", args(&["--no-newline", "--type", "text"]), vec!["wl-paste"]),
        ("DISPLAY", "xclip", args(&["-selection", "clipboard", "-out"]), vec!["xclip"]),
        ("DISPLAY", "xsel", args(&["--clipboard", "--output"]), vec!["xclip", "xsel"]),
    ] {
        for text in ["clipboard text", ""] {
            let runner = Mock::new(move |name, _| (name == command).then(|| text.as_bytes().to_vec()));
            let (result, _) =
                with(env(Platform::Linux, &[("DISPLAY", ":0"), (var, "1")]), &runner, read_clipboard_text_with);
            assert_eq!(result, Some(text.to_string()).filter(|t| !t.is_empty()));
            assert_eq!(runner.names(), calls);
            assert_eq!(runner.call(calls.len() - 1).1, expected_args);
        }
    }
    let runner = Mock::failing();
    let vars = [("DISPLAY", ":0"), ("WAYLAND_DISPLAY", "wayland-0")];
    assert_eq!(with(env(Platform::Linux, &vars), &runner, read_clipboard_text_with).0, None);
    assert_eq!(runner.names(), ["wl-paste", "xclip", "xsel"]);
}

#[test]
fn read_text_on_termux_macos_and_windows() {
    let runner = Mock::new(|_, _| Some(b"termux".to_vec()));
    let result = with(env(Platform::Android, &[("TERMUX_VERSION", "0.119")]), &runner, read_clipboard_text_with).0;
    assert_eq!((result.as_deref(), runner.names()), (Some("termux"), vec!["termux-clipboard-get".to_string()]));

    let runner = Mock::new(|_, _| Some("mac \u{e9}".as_bytes().to_vec()));
    assert_eq!(with(env(Platform::MacOs, &[]), &runner, read_clipboard_text_with).0.as_deref(), Some("mac \u{e9}"));
    assert_eq!(runner.names(), ["pbpaste"]);

    let runner = Mock::new(|_, _| Some(b"win".to_vec()));
    assert_eq!(with(env(Platform::Windows, &[]), &runner, read_clipboard_text_with).0.as_deref(), Some("win"));
    let (name, script_args, _) = runner.call(0);
    assert_eq!(name, "powershell.exe");
    assert!(script_args[2].contains("Get-Clipboard -Raw"));
}

#[test]
fn copy_uses_the_platform_writer() {
    let runner = Mock::ok();
    let (result, osc) = copy(env(Platform::MacOs, &[]), &runner, "hello");
    assert!(result.is_ok() && osc.is_empty());
    assert_eq!(runner.call(0), ("pbcopy".into(), vec![], Some("hello".into())));

    let runner = Mock::ok();
    copy(env(Platform::Linux, &[("DISPLAY", ":0")]), &runner, "hello").0.unwrap();
    assert_eq!(runner.call(0), ("xclip".into(), args(&["-selection", "clipboard"]), Some("hello".into())));

    let runner = Mock::new(|name, _| (name == "xsel").then(Vec::new));
    let vars = [("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")];
    let (result, osc) = copy(env(Platform::Linux, &vars), &runner, "hello");
    assert!(result.is_ok() && osc.is_empty());
    assert_eq!(runner.names(), ["wl-copy", "xclip", "xsel"]);

    let runner = Mock::ok();
    copy(env(Platform::Android, &[("TERMUX_VERSION", "0.119")]), &runner, "hello").0.unwrap();
    assert_eq!(runner.call(0), ("termux-clipboard-set".into(), vec![], Some("hello".into())));
}

#[test]
fn copy_on_windows_prefers_powershell_then_clip() {
    let runner = Mock::ok();
    copy(env(Platform::Windows, &[]), &runner, "h\u{e9}llo").0.unwrap();
    let (name, script_args, _) = runner.call(0);
    assert_eq!((name.as_str(), runner.names().len()), ("powershell.exe", 1));
    assert!(script_args[2].starts_with("Set-Clipboard -Value ([System.IO.File]::ReadAllText('"));

    let runner = Mock::new(|name, _| (name == "clip").then(Vec::new));
    copy(env(Platform::Windows, &[]), &runner, "hello").0.unwrap();
    assert_eq!(runner.names(), ["powershell.exe", "clip"]);
}

#[test]
fn copy_reports_failures_and_uses_osc52_when_appropriate() {
    let runner = Mock::failing();
    let (result, osc) = copy(env(Platform::Linux, &[("DISPLAY", ":0")]), &runner, "hello");
    assert_eq!(
        result.unwrap_err().to_string(),
        "Clipboard unavailable: install `xclip` or `xsel`, or check X11 access"
    );
    assert!(osc.is_empty());

    let vars = [("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")];
    let (result, _) = copy(env(Platform::Linux, &vars), &Mock::failing(), "hello");
    let message = "Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access";
    assert_eq!(result.unwrap_err().to_string(), message);

    let (result, _) = copy(env(Platform::Android, &[("TERMUX_VERSION", "1")]), &Mock::failing(), "hello");
    let message = "Clipboard unavailable: install the Termux:API app and `termux-api` package";
    assert_eq!(result.unwrap_err().to_string(), message);

    let runner = Mock::ok();
    let (result, osc) = copy(env(Platform::Linux, &[]), &runner, "hello");
    assert!(result.is_ok() && runner.names().is_empty());
    assert_eq!(osc, ["\x1b]52;c;aGVsbG8=\x07"]);

    let ssh = [("SSH_CONNECTION", "client server")];
    let (result, osc) = copy(env(Platform::MacOs, &ssh), &Mock::ok(), "hello");
    assert!(result.is_ok());
    assert_eq!(osc.len(), 1, "remote sessions also reach the client clipboard");
    let (result, osc) = copy(env(Platform::MacOs, &ssh), &Mock::failing(), "hello");
    assert!(result.is_ok() && osc.len() == 1);
    let (result, osc) = copy(env(Platform::MacOs, &ssh), &Mock::failing(), &"x".repeat(80_000));
    assert_eq!(result.unwrap_err().to_string(), "Clipboard unavailable: text exceeds the OSC 52 size limit");
    assert!(osc.is_empty());
    let (result, _) = copy(env(Platform::MacOs, &[]), &Mock::failing(), "hello");
    assert_eq!(result.unwrap_err().to_string(), "Clipboard unavailable");
}

#[test]
fn wsl_without_a_display_writes_through_powershell() {
    let written = std::sync::Arc::new(Mutex::new(None::<(String, String)>));
    let seen = written.clone();
    let runner = Mock::new(move |name, args| {
        if name == "wslpath" {
            let text = std::fs::read_to_string(&args[1]).unwrap();
            *seen.lock().unwrap() = Some((args[1].clone(), text));
            return Some(b"\\\\wsl.localhost\\Ubuntu\\tmp\\clip's.txt\n".to_vec());
        }
        Some(Vec::new())
    });
    let wsl = [("WSL_DISTRO_NAME", "Ubuntu")];
    let (result, osc) = copy(env(Platform::Linux, &wsl), &runner, "h\u{e9}llo");
    assert!(result.is_ok() && osc.is_empty());
    assert_eq!(runner.names(), ["wslpath", "powershell.exe"]);
    let (path, text) = written.lock().unwrap().clone().unwrap();
    assert_eq!(text, "h\u{e9}llo");
    assert!(!std::path::Path::new(&path).exists(), "the temporary file is removed");
    let script = &runner.call(1).1[2];
    assert!(script.contains("Set-Clipboard") && script.contains("'\\\\wsl.localhost\\Ubuntu\\tmp\\clip''s.txt'"));

    let runner = Mock::failing();
    let (result, osc) = copy(env(Platform::Linux, &wsl), &runner, "hello");
    assert!(result.is_ok() && osc.len() == 1);
    assert_eq!(runner.names(), ["wslpath"]);

    let terminal = [("WSL_DISTRO_NAME", "Ubuntu"), ("WT_SESSION", "s"), ("SSH_CONNECTION", "c s")];
    let runner = Mock::ok();
    let (_, osc) = copy(env(Platform::Linux, &terminal), &runner, "hello");
    assert_eq!((osc.len(), runner.names().len()), (1, 0));

    let runner = Mock::new(|name, _| Some(if name == "wslpath" { b"C:\\clip.txt".to_vec() } else { Vec::new() }));
    let (_, osc) = copy(env(Platform::Linux, &terminal[..2]), &runner, &"x".repeat(80_000));
    assert_eq!((osc.len(), runner.names()), (0, vec!["wslpath".to_string(), "powershell.exe".into()]));

    let runner = Mock::ok();
    copy(env(Platform::Linux, &[("WSL_DISTRO_NAME", "Ubuntu"), ("WAYLAND_DISPLAY", "w")]), &runner, "x").0.unwrap();
    assert_eq!(runner.names(), ["wl-copy"]);
}

#[test]
fn detects_wsl_from_env_or_proc_version() {
    assert!(is_wsl(&env(Platform::Linux, &[("WSLENV", "x")])));
    assert!(!is_wsl(&env(Platform::Linux, &[("WSLENV", "")])));
    let mut kernel = env(Platform::Linux, &[]);
    kernel.proc_version = Some("Linux version 5.15.90.1-Microsoft-standard-WSL2".into());
    assert!(is_wsl(&kernel));
    kernel.proc_version = Some("Linux version 6.8.0-generic".into());
    assert!(!is_wsl(&kernel));
}

#[test]
fn reads_images_from_wayland_x11_and_native_helpers() {
    let png = crate::images::encode_png(&::image::RgbaImage::new(2, 2)).unwrap();
    let data = png.clone();
    let runner = Mock::new(move |_, args| {
        Some(if args[0] == "--list-types" { b"text/plain\nimage/png\n".to_vec() } else { data.clone() })
    });
    let image = with(env(Platform::Linux, &[("WAYLAND_DISPLAY", "w")]), &runner, read_clipboard_image_with).0;
    assert_eq!(image, Some(ClipboardImage { bytes: png.clone(), mime_type: "image/png".into() }));
    assert_eq!(runner.call(1).1, args(&["--type", "image/png", "--no-newline"]));

    // An empty Wayland clipboard must not fall through to stale X11 contents.
    let runner = Mock::new(|_, _| Some(b"text/plain\n".to_vec()));
    assert_eq!(
        with(env(Platform::Linux, &[("XDG_SESSION_TYPE", "wayland")]), &runner, read_clipboard_image_with).0,
        None
    );
    assert_eq!(runner.names(), ["wl-paste"]);

    let mut bmp = Vec::new();
    ::image::DynamicImage::ImageRgba8(::image::RgbaImage::new(3, 1))
        .write_to(&mut std::io::Cursor::new(&mut bmp), ::image::ImageFormat::Bmp)
        .unwrap();
    let runner = Mock::new(move |_, args| {
        Some(if args.contains(&"TARGETS".to_string()) { b"TARGETS\nimage/bmp\n".to_vec() } else { bmp.clone() })
    });
    let image = with(env(Platform::Linux, &[("DISPLAY", ":0")]), &runner, read_clipboard_image_with).0.unwrap();
    assert_eq!(image.mime_type, "image/png");
    assert_eq!(::image::load_from_memory(&image.bytes).unwrap().width(), 3);

    let hex: String = png.iter().map(|byte| format!("{byte:02X}")).collect();
    let runner = Mock::new(move |_, _| Some(format!("\u{ab}data PNGf{hex}\u{bb}\n").into_bytes()));
    let image = with(env(Platform::MacOs, &[]), &runner, read_clipboard_image_with).0.unwrap();
    assert_eq!((image.bytes, image.mime_type.as_str()), (png, "image/png"));
    assert_eq!(runner.call(0).0, "osascript");

    let runner = Mock::ok();
    assert_eq!(with(env(Platform::Android, &[("TERMUX_VERSION", "1")]), &runner, read_clipboard_image_with).0, None);
    assert!(runner.names().is_empty());
    assert_eq!(extension_for_image_mime_type("image/jpeg; q=1"), Some("jpg"));
    assert_eq!(extension_for_image_mime_type("image/bmp"), None);
}

#[test]
fn wsl_reads_windows_screenshots_through_powershell() {
    let png = crate::images::encode_png(&::image::RgbaImage::new(1, 1)).unwrap();
    let data = png.clone();
    let runner = Mock::new(move |name, args| match name {
        "wl-paste" => Some(b"text/plain\n".to_vec()),
        "wslpath" => Some(args[1].clone().into_bytes()),
        "powershell.exe" => {
            let path = args[2].split("$path = '").nth(1)?.split('\'').next()?.to_string();
            std::fs::write(path, &data).ok()?;
            Some(b"ok\r\n".to_vec())
        }
        _ => None,
    });
    let image = with(env(Platform::Linux, &[("WSL_DISTRO_NAME", "Ubuntu")]), &runner, read_clipboard_image_with).0;
    assert_eq!(image, Some(ClipboardImage { bytes: png, mime_type: "image/png".into() }));
    assert_eq!(runner.names(), ["wl-paste", "wslpath", "powershell.exe"]);
}

#[cfg(unix)]
#[test]
fn command_runner_distinguishes_failure_empty_output_and_timeouts() {
    let sh = |script: &str| vec!["-c".to_string(), script.to_string()];
    let timeout = Duration::from_secs(5);
    assert_eq!(run_clipboard_command("sh", &sh("printf 'a\\000b'"), None, timeout), Some(b"a\0b".to_vec()));
    assert_eq!(run_clipboard_command("sh", &sh("true"), None, timeout), Some(Vec::new()));
    assert_eq!(run_clipboard_command("sh", &sh("exit 1"), None, timeout), None);
    assert_eq!(run_clipboard_command("pi-no-such-command", &[], None, timeout), None);
    let check = sh("test \"$(cat)\" = 'caf\u{e9} \u{65e5}\u{672c}\u{8a9e}'");
    assert!(
        run_clipboard_command("sh", &check, Some("caf\u{e9} \u{65e5}\u{672c}\u{8a9e}".as_bytes()), timeout).is_some()
    );
    let started = std::time::Instant::now();
    assert_eq!(run_clipboard_command("sh", &sh("sleep 5"), None, Duration::from_millis(100)), None);
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn reads_copied_file_paths_on_macos_and_windows() {
    let runner = Mock::new(|_, _| Some(b"/Users/me/a.png\n/Users/me/b c.txt\n".to_vec()));
    let paths = with(env(Platform::MacOs, &[]), &runner, read_clipboard_file_paths_with).0;
    assert_eq!(paths.unwrap(), ["/Users/me/a.png", "/Users/me/b c.txt"]);
    let (name, script_args, _) = runner.call(0);
    assert_eq!((name.as_str(), &script_args[..3]), ("osascript", &args(&["-l", "JavaScript", "-e"])[..]));

    let runner = Mock::new(|_, _| Some(b"C:\\a.png\r\n".to_vec()));
    let paths = with(env(Platform::Windows, &[]), &runner, read_clipboard_file_paths_with).0;
    assert_eq!(paths.unwrap(), ["C:\\a.png"]);
    assert!(runner.call(0).1[2].contains("FileDropList"));

    let runner = Mock::new(|_, _| Some(Vec::new()));
    assert_eq!(with(env(Platform::MacOs, &[]), &runner, read_clipboard_file_paths_with).0, None);
    let runner = Mock::ok();
    assert_eq!(with(env(Platform::Linux, &[("DISPLAY", ":0")]), &runner, read_clipboard_file_paths_with).0, None);
    assert!(runner.names().is_empty());
}

#[test]
fn saves_pasted_images_with_their_extension() {
    let image = ClipboardImage { bytes: vec![1, 2, 3], mime_type: "image/jpeg".into() };
    let path = save_clipboard_image(&image).unwrap();
    let name = path.file_name().unwrap().to_str().unwrap().to_string();
    assert!(name.starts_with("pi-clipboard-") && name.ends_with(".jpg"), "{name}");
    assert_eq!(std::fs::read(&path).unwrap(), [1, 2, 3]);
    std::fs::remove_file(path).unwrap();
}
