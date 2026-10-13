//! Which loaded extensions appear in a JavaScript stack trace, for crash attribution
//! (upstream `findExtensionStackMatches` in `core/crash-log.ts`).

use std::sync::LazyLock;

use regex::Regex;

/// The parts of a loaded extension that stack matching needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtensionStackInfo {
    pub path: String,
    pub resolved_path: String,
    pub source: String,
    /// `package`, `top-level`, and so on.
    pub origin: String,
    pub base_dir: Option<String>,
}

static FRAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+at\s").expect("valid pattern"));
static REMOTE_SOURCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:npm:|git:|https?://|ssh://)").expect("valid pattern"));
static SCRIPT_FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.[cm]?[jt]s$").expect("valid pattern"));
static INDEX_FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/index\.[cm]?[jt]s$").expect("valid pattern"));

fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_string()
}

fn is_synthetic(path: &str) -> bool {
    path.starts_with("builtin:") || path.starts_with('<')
}

/// JavaScript `decodeURI`: percent escapes decode except reserved characters; malformed
/// input returns `None`.
fn decode_uri(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
        let byte = u8::from_str_radix(hex, 16).ok()?;
        if b"#$&+,/:;=?@".contains(&byte) {
            decoded.extend_from_slice(&bytes[index..index + 3]);
        } else {
            decoded.push(byte);
        }
        index += 3;
    }
    String::from_utf8(decoded).ok()
}

fn contains_path(stack: &str, target: &str, descendants: bool) -> bool {
    let target = normalize(target);
    if target.is_empty() || is_synthetic(&target) {
        return false;
    }
    let bytes = target.as_bytes();
    let windows_drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b":/";
    let (haystack, needle) =
        if windows_drive { (stack.to_lowercase(), target.to_lowercase()) } else { (stack.to_string(), target) };
    if descendants {
        return haystack.contains(&format!("{needle}/"));
    }
    haystack.match_indices(&needle).any(|(index, _)| {
        let next = haystack[index + needle.len()..].chars().next();
        next.is_none_or(|ch| ch == ':' || ch == ')' || ch.is_whitespace())
    })
}

/// An error from JavaScript with its stack, shown by its message (upstream `Error.stack`).
#[derive(Debug, Clone)]
pub struct StackError {
    pub message: String,
    pub stack: Option<String>,
}

impl std::fmt::Display for StackError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StackError {}

/// The JavaScript stack in `error`'s chain, else its debug text.
pub fn error_stack(error: &anyhow::Error) -> String {
    let stack = error.chain().find_map(|cause| cause.downcast_ref::<StackError>()).and_then(|e| e.stack.clone());
    stack.unwrap_or_else(|| format!("{error:?}"))
}

/// Labels (package source or extension path) of extensions with files in the stack frames.
pub fn find_extension_stack_matches(stack: Option<&str>, extensions: &[ExtensionStackInfo]) -> Vec<String> {
    let Some(stack) = stack else { return Vec::new() };
    let frames: Vec<String> = stack
        .split('\n')
        .skip(1)
        .filter(|line| FRAME.is_match(line))
        .map(|line| decode_uri(line).unwrap_or_else(|| line.to_string()))
        .collect();
    let frames = frames.join("\n").replace('\\', "/");
    let mut matches: Vec<String> = Vec::new();
    for extension in extensions {
        let resolved = normalize(&extension.resolved_path);
        let package = extension.origin == "package";
        let single_file =
            package && !REMOTE_SOURCE.is_match(&extension.source) && SCRIPT_FILE.is_match(&extension.source);
        let package_root = extension.base_dir.as_deref().filter(|_| package && !single_file);
        let matched = match (package_root, resolved.rfind('/')) {
            (Some(root), _) => contains_path(&frames, root, true),
            (None, Some(slash)) if INDEX_FILE.is_match(&resolved) => contains_path(&frames, &resolved[..slash], true),
            (None, _) => contains_path(&frames, &resolved, false),
        };
        if !matched {
            continue;
        }
        let label = if package && !extension.source.is_empty() { &extension.source } else { &extension.path };
        if !matches.contains(label) {
            matches.push(label.clone());
        }
    }
    matches
}

/// The warning shown after a fatal error whose stack has frames from loaded extensions
/// (upstream `formatCrashExtensionHint`).
pub fn format_crash_extension_hint(matches: &[String]) -> Option<String> {
    let quoted: Vec<String> = matches.iter().filter(|m| !m.is_empty()).map(|m| format!("`{m}`")).collect();
    let labels = match quoted.as_slice() {
        [] => return None,
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    };
    let (noun, pronoun) = if quoted.len() == 1 { ("extension", "it") } else { ("extensions", "them") };
    let app = crate::resources::brand::APP_NAME;
    Some(format!(
        "A stack frame came from loaded {noun} {labels}, which may be involved. Try disabling {pronoun} with `{app} config`, or run `{app} -ne` to confirm."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(source: &str, base_dir: &str, entry: &str) -> ExtensionStackInfo {
        let resolved = format!("{}/{entry}", base_dir.replace('\\', "/"));
        ExtensionStackInfo {
            path: resolved.clone(),
            resolved_path: resolved,
            source: source.into(),
            origin: "package".into(),
            base_dir: Some(base_dir.into()),
        }
    }

    #[test]
    fn matches_frames_beneath_package_roots() {
        let memory = package(
            "npm:pi-observational-memory",
            "/home/f/.pi/agent/npm/node_modules/pi-observational-memory",
            "extensions/index.ts",
        );
        let unrelated = package("npm:unrelated", "/home/f/.pi/agent/npm/node_modules/unrelated", "extensions/index.ts");
        let stack = "TypeError: Cannot read properties of undefined (reading 'runtime')\n    at streamSimple (file:///home/f/.local/lib/node_modules/pi/dist/chunk.js:1093:16944)\n    at /home/f/.pi/agent/npm/node_modules/pi-observational-memory/src/agents/worker-stream.ts:43:45";
        assert_eq!(find_extension_stack_matches(Some(stack), &[memory, unrelated]), ["npm:pi-observational-memory"]);
    }

    #[test]
    fn normalizes_windows_paths_and_deduplicates_packages() {
        let root = r"C:\Users\reporter\.pi\agent\npm\node_modules\@scope\memory";
        let first = package("npm:@scope/memory", root, "extensions/first.ts");
        let second = package("npm:@scope/memory", root, "extensions/second.ts");
        let stack = "Error: broken\n    at run (c:\\users\\reporter\\.pi\\agent\\npm\\node_modules\\@scope\\memory\\src\\worker.ts:4:2)";
        assert_eq!(find_extension_stack_matches(Some(stack), &[first, second]), ["npm:@scope/memory"]);
    }

    #[test]
    fn single_file_packages_and_error_text_do_not_match() {
        let single = |name: &str| ExtensionStackInfo {
            path: format!("/plugins/{name}.ts"),
            resolved_path: format!("/plugins/{name}.ts"),
            source: format!("/plugins/{name}.ts"),
            origin: "package".into(),
            base_dir: Some("/plugins".into()),
        };
        let stack = "Error: broken\n    at run (file:///plugins/b.ts:4:2)";
        assert_eq!(find_extension_stack_matches(Some(stack), &[single("a"), single("b")]), ["/plugins/b.ts"]);
        let memory = package("npm:memory", "/tmp/node_modules/memory", "extensions/index.ts");
        let stack =
            format!("Error: Failed to read {}\n    at run (file:///opt/pi/dist/core.js:4:2)", memory.resolved_path);
        assert!(find_extension_stack_matches(Some(&stack), &[memory]).is_empty());
        assert!(find_extension_stack_matches(None, &[]).is_empty());
    }

    #[test]
    fn decodes_frame_paths_independently_from_malformed_error_text() {
        let path = "/Users/reporter/.pi/agent/extensions/local memory/index.ts";
        let extension = ExtensionStackInfo {
            path: path.into(),
            resolved_path: path.into(),
            source: "local".into(),
            origin: "top-level".into(),
            base_dir: Some("/Users/reporter/.pi/agent/extensions".into()),
        };
        let stack = "Error: progress 100%\n    at run (file:///Users/reporter/.pi/agent/extensions/local%20memory/worker.ts:4:2)";
        assert_eq!(find_extension_stack_matches(Some(stack), &[extension]), [path]);
    }

    #[test]
    fn formats_the_extension_hint_like_upstream() {
        let labels = |items: &[&str]| items.iter().map(|item| item.to_string()).collect::<Vec<_>>();
        assert_eq!(format_crash_extension_hint(&[]), None);
        let app = crate::resources::brand::APP_NAME;
        assert_eq!(
            format_crash_extension_hint(&labels(&["npm:a"])).unwrap(),
            format!(
                "A stack frame came from loaded extension `npm:a`, which may be involved. Try disabling it with `{app} config`, or run `{app} -ne` to confirm."
            )
        );
        assert!(format_crash_extension_hint(&labels(&["a", "b"])).unwrap().contains("extensions `a` and `b`,"));
        assert!(format_crash_extension_hint(&labels(&["a", "b", "c"])).unwrap().contains("`a`, `b`, and `c`,"));
    }
}
