//! Native clipboard access and modifier-key probes.
//! macOS and Windows use the system clipboard directly; Linux reads X11 when `DISPLAY` is set.
//! Modifier probes exist on macOS (CoreGraphics flags) and Windows (`GetAsyncKeyState`) only.

use std::fmt;
use std::sync::Arc;

use crate::env::{EnvLookup, ProcessEnv};

/// A modifier key that [`is_native_modifier_pressed`] can probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModifierKey {
    /// Shift.
    Shift,
    /// Command on macOS, the Windows key on Windows.
    Command,
    /// Control.
    Control,
    /// Option on macOS, Alt on Windows.
    Option,
}

impl ModifierKey {
    /// The upstream name of the key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shift => "shift",
            Self::Command => "command",
            Self::Control => "control",
            Self::Option => "option",
        }
    }
}

/// The result of a clipboard read: `Unavailable` means the clipboard or format cannot be read here,
/// `Empty` means it holds no content of that kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent<T> {
    /// The clipboard cannot be reached, or the read is not supported on this platform.
    Unavailable,
    /// The clipboard holds no content of the requested kind.
    Empty,
    /// The content.
    Data(T),
}

impl<T> ClipboardContent<T> {
    /// The content, if there is any.
    pub fn data(self) -> Option<T> {
        match self {
            Self::Data(value) => Some(value),
            Self::Unavailable | Self::Empty => None,
        }
    }

    /// Whether the read could not reach the clipboard.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

/// A clipboard transfer that failed after the clipboard was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardError(pub String);

impl fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClipboardError {}

/// Native clipboard access. Reads block until the system answers; run them off the UI thread.
pub trait NativeClipboard: Send + Sync {
    /// Text on the clipboard. Transfer failures are errors.
    fn get_text(&self) -> Result<ClipboardContent<String>, ClipboardError>;

    /// The clipboard image as PNG bytes. Transfer failures are errors.
    fn get_image(&self) -> Result<ClipboardContent<Vec<u8>>, ClipboardError>;

    /// Paths of files on the clipboard. `Unavailable` means this platform does not support it.
    fn get_file_paths(&self) -> Result<ClipboardContent<Vec<String>>, ClipboardError> {
        Ok(ClipboardContent::Unavailable)
    }

    /// Whether [`NativeClipboard::set_text`] works. Linux uses command-line tools instead,
    /// because they keep clipboard ownership after the write returns.
    fn supports_set_text(&self) -> bool {
        false
    }

    /// Replaces the clipboard text.
    fn set_text(&self, text: &str) -> Result<(), ClipboardError> {
        let _ = text;
        Err(ClipboardError("Setting clipboard text is not supported".to_owned()))
    }
}

/// The native clipboard, or `None` when this platform has none.
/// On Linux it needs `DISPLAY`; the display is opened only when a read is requested.
pub fn get_native_clipboard() -> Option<Arc<dyn NativeClipboard>> {
    get_native_clipboard_with_env(&ProcessEnv)
}

/// Like [`get_native_clipboard`], reading `DISPLAY` from `env`. Repeated calls return the same instance.
pub fn get_native_clipboard_with_env(env: &dyn EnvLookup) -> Option<Arc<dyn NativeClipboard>> {
    if cfg!(target_os = "linux") && env.non_empty("DISPLAY").is_none() {
        return None;
    }
    system::clipboard()
}

/// Whether `key` is held down right now, as the operating system reports it.
/// Returns false where no probe exists (Linux and other systems).
pub fn is_native_modifier_pressed(key: ModifierKey) -> bool {
    modifiers::is_pressed(key)
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod system {
    use std::sync::{Arc, OnceLock};

    use super::{ClipboardContent, ClipboardError, NativeClipboard};

    /// arboard-backed clipboard. Each read opens a new connection, so a lost display can recover.
    struct SystemClipboard;

    fn open() -> Option<arboard::Clipboard> {
        arboard::Clipboard::new().ok()
    }

    fn content<T>(result: Result<T, arboard::Error>) -> Result<ClipboardContent<T>, ClipboardError> {
        match result {
            Ok(value) => Ok(ClipboardContent::Data(value)),
            Err(arboard::Error::ContentNotAvailable) => Ok(ClipboardContent::Empty),
            Err(arboard::Error::ClipboardNotSupported) => Ok(ClipboardContent::Unavailable),
            Err(error) => Err(ClipboardError(error.to_string())),
        }
    }

    fn png_error(error: png::EncodingError) -> ClipboardError {
        ClipboardError(format!("Could not encode clipboard image: {error}"))
    }

    fn encode_png(image: &arboard::ImageData<'_>) -> Result<Vec<u8>, ClipboardError> {
        let too_large = || ClipboardError("Clipboard image is too large".to_owned());
        let width = u32::try_from(image.width).map_err(|_| too_large())?;
        let height = u32::try_from(image.height).map_err(|_| too_large())?;
        let mut output = Vec::new();
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(png_error)?;
        writer.write_image_data(&image.bytes).map_err(png_error)?;
        writer.finish().map_err(png_error)?;
        Ok(output)
    }

    impl NativeClipboard for SystemClipboard {
        fn get_text(&self) -> Result<ClipboardContent<String>, ClipboardError> {
            let Some(mut clipboard) = open() else {
                return Ok(ClipboardContent::Unavailable);
            };
            content(clipboard.get_text())
        }

        fn get_image(&self) -> Result<ClipboardContent<Vec<u8>>, ClipboardError> {
            let Some(mut clipboard) = open() else {
                return Ok(ClipboardContent::Unavailable);
            };
            match content(clipboard.get_image())? {
                ClipboardContent::Data(image) => encode_png(&image).map(ClipboardContent::Data),
                ClipboardContent::Empty => Ok(ClipboardContent::Empty),
                ClipboardContent::Unavailable => Ok(ClipboardContent::Unavailable),
            }
        }

        fn get_file_paths(&self) -> Result<ClipboardContent<Vec<String>>, ClipboardError> {
            if !cfg!(target_os = "macos") {
                return Ok(ClipboardContent::Unavailable);
            }
            let Some(mut clipboard) = open() else {
                return Ok(ClipboardContent::Unavailable);
            };
            Ok(match content(clipboard.get().file_list())? {
                ClipboardContent::Data(paths) if !paths.is_empty() => {
                    ClipboardContent::Data(paths.into_iter().map(|path| path.to_string_lossy().into_owned()).collect())
                }
                ClipboardContent::Unavailable => ClipboardContent::Unavailable,
                ClipboardContent::Data(_) | ClipboardContent::Empty => ClipboardContent::Empty,
            })
        }

        fn supports_set_text(&self) -> bool {
            !cfg!(target_os = "linux")
        }

        fn set_text(&self, text: &str) -> Result<(), ClipboardError> {
            if !self.supports_set_text() {
                return Err(ClipboardError("Setting clipboard text is not supported".to_owned()));
            }
            let mut clipboard = arboard::Clipboard::new().map_err(|error| ClipboardError(error.to_string()))?;
            clipboard.set_text(text).map_err(|error| ClipboardError(error.to_string()))
        }
    }

    static CLIPBOARD: OnceLock<Arc<dyn NativeClipboard>> = OnceLock::new();

    pub(super) fn clipboard() -> Option<Arc<dyn NativeClipboard>> {
        Some(Arc::clone(CLIPBOARD.get_or_init(|| Arc::new(SystemClipboard))))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod system {
    use std::sync::Arc;

    use super::NativeClipboard;

    pub(super) fn clipboard() -> Option<Arc<dyn NativeClipboard>> {
        None
    }
}

#[cfg(target_os = "macos")]
mod modifiers {
    use objc2_core_graphics::{CGEventFlags, CGEventSource, CGEventSourceStateID};

    use super::ModifierKey;

    pub(super) fn is_pressed(key: ModifierKey) -> bool {
        let mask = match key {
            ModifierKey::Shift => CGEventFlags::MaskShift,
            ModifierKey::Command => CGEventFlags::MaskCommand,
            ModifierKey::Control => CGEventFlags::MaskControl,
            ModifierKey::Option => CGEventFlags::MaskAlternate,
        };
        CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState).intersects(mask)
    }
}

#[cfg(windows)]
mod modifiers {
    use winsafe::GetAsyncKeyState;
    use winsafe::co::VK;

    use super::ModifierKey;

    pub(super) fn is_pressed(key: ModifierKey) -> bool {
        let keys: &[VK] = match key {
            ModifierKey::Shift => &[VK::SHIFT, VK::LSHIFT, VK::RSHIFT],
            ModifierKey::Control => &[VK::CONTROL, VK::LCONTROL, VK::RCONTROL],
            ModifierKey::Option => &[VK::MENU, VK::LMENU, VK::RMENU],
            ModifierKey::Command => &[VK::LWIN, VK::RWIN],
        };
        keys.iter().copied().any(GetAsyncKeyState)
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod modifiers {
    use super::ModifierKey;

    pub(super) fn is_pressed(_key: ModifierKey) -> bool {
        false
    }
}
