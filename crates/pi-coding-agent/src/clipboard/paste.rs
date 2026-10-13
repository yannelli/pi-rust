//! Paste helpers: copied file paths (Finder or Explorer copies) and saving pasted images
//! to temporary files, as the interactive editor inserts them (upstream `handleClipboardPaste`).

use std::path::PathBuf;

use super::image::{ClipboardImage, extension_for_image_mime_type};
use super::{ClipboardContext, Platform, run, with_system_context};
use crate::export::session_file::random_hex;

/// JXA script that prints the file URLs on the macOS pasteboard, one path per line.
const MACOS_FILE_PATHS_SCRIPT: &str = "ObjC.import('AppKit');\
var pb = $.NSPasteboard.generalPasteboard;\
var items = pb.readObjectsForClassesOptions($([$.NSURL]), $({NSPasteboardURLReadingFileURLsOnlyKey: true}));\
var out = [];\
if (items) { for (var i = 0; i < items.count; i++) { out.push(items.objectAtIndex(i).path.js); } }\
out.join('\\n');";

const WINDOWS_FILE_PATHS_SCRIPT: &str = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; \
Get-Clipboard -Format FileDropList | ForEach-Object { [Console]::Out.WriteLine($_.FullName) }";

/// File paths copied in the file manager (macOS and Windows); `None` when there are none.
pub fn read_clipboard_file_paths() -> Option<Vec<String>> {
    with_system_context(read_clipboard_file_paths_with)
}

pub fn read_clipboard_file_paths_with(context: &ClipboardContext<'_>) -> Option<Vec<String>> {
    let output = match context.env.platform {
        Platform::MacOs => run(context, "osascript", &["-l", "JavaScript", "-e", MACOS_FILE_PATHS_SCRIPT], None),
        Platform::Windows => {
            run(context, "powershell.exe", &["-NoProfile", "-Command", WINDOWS_FILE_PATHS_SCRIPT], None)
        }
        _ => None,
    }?;
    let text = String::from_utf8_lossy(&output);
    let paths: Vec<String> = text
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    (!paths.is_empty()).then_some(paths)
}

/// Write a pasted image to `<temp>/pi-clipboard-<id>.<ext>` and return the path.
pub fn save_clipboard_image(image: &ClipboardImage) -> std::io::Result<PathBuf> {
    let extension = extension_for_image_mime_type(&image.mime_type).unwrap_or("png");
    let path = std::env::temp_dir().join(format!("pi-clipboard-{}.{extension}", random_hex(32)));
    std::fs::write(&path, &image.bytes)?;
    Ok(path)
}
