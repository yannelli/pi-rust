//! Windows path rules the daemon applies to the shell and `taskkill` locations. They are plain string functions so they
//! also run in tests on other systems.

/// Node's `path.win32.normalize` for drive and rooted paths: `/` becomes `\`, empty and `.`
/// segments drop, and `..` removes the previous segment.
fn win32_normalize(path: &str) -> String {
    let path = path.replace('/', "\\");
    let bytes = path.as_bytes();
    let drive = if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' { &path[..2] } else { "" };
    let after_drive = &path[drive.len()..];
    let rooted = after_drive.starts_with('\\');
    let mut segments: Vec<&str> = Vec::new();
    for segment in after_drive.split('\\') {
        match segment {
            "" | "." => {}
            ".." => match segments.last() {
                Some(&last) if last != ".." => {
                    segments.pop();
                }
                _ if rooted => {}
                _ => segments.push(".."),
            },
            _ => segments.push(segment),
        }
    }
    let mut tail = segments.join("\\");
    if tail.is_empty() {
        if !rooted {
            tail.push('.');
        }
    } else if after_drive.ends_with('\\') {
        tail.push('\\');
    }
    format!("{drive}{}{tail}", if rooted { "\\" } else { "" })
}

/// The legacy WSL launcher, `bash.exe` under `System32` or `Sysnative` of `system_root`, reads
/// commands from stdin. Without `system_root` only the conventional `C:\Windows` paths count.
pub fn is_legacy_wsl_bash(path: &str, system_root: Option<&str>) -> bool {
    let normalized = win32_normalize(path).to_lowercase();
    if let Some(root) = system_root.filter(|root| !root.is_empty()) {
        if !matches!(root.as_bytes(), [letter, b':', b'\\' | b'/', ..] if letter.is_ascii_alphabetic()) {
            return false;
        }
        return ["System32", "Sysnative"]
            .iter()
            .any(|directory| normalized == win32_normalize(&format!("{root}\\{directory}\\bash.exe")).to_lowercase());
    }
    matches!(
        normalized.as_bytes(),
        [b'a'..=b'z', b':', b'\\', rest @ ..]
            if rest == b"windows\\system32\\bash.exe" || rest == b"windows\\sysnative\\bash.exe"
    )
}

/// `%SystemRoot%`, or `C:\Windows` when it is unset or empty.
pub fn windows_directory(system_root: Option<String>) -> String {
    system_root.filter(|root| !root.is_empty()).unwrap_or_else(|| "C:\\Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_wsl_launchers_follow_the_windows_directory() {
        let cases = [
            ("C:\\Windows\\System32\\bash.exe", None, true),
            ("c:/windows/sysnative/BASH.EXE", None, true),
            ("D:\\WinNT\\System32\\bash.exe", Some("D:\\WinNT"), true),
            ("D:\\WinNT\\Sysnative\\bash.exe", Some("D:\\WinNT"), true),
            ("D:/WinNT/./System32/../System32/bash.exe", Some("D:\\WinNT\\"), true),
            ("D:\\WinNT\\System32\\bash.exe", Some(""), false),
            ("D:\\WinNT\\System32\\bash.exe", None, false),
            ("C:\\Windows\\System32\\bash.exe", Some("D:\\WinNT"), false),
            ("E:\\Windows\\System32\\bash.exe", Some("D:\\WinNT"), false),
            ("D:\\WinNT\\System32\\bash.exe", Some("WinNT"), false),
            ("C:\\Git\\bin\\bash.exe", None, false),
        ];
        for (path, root, expected) in cases {
            assert_eq!(is_legacy_wsl_bash(path, root), expected, "{path} with SystemRoot={root:?}");
        }
    }

    #[test]
    fn an_empty_system_root_falls_back_to_the_default_windows_directory() {
        assert_eq!(windows_directory(None), "C:\\Windows");
        assert_eq!(windows_directory(Some(String::new())), "C:\\Windows");
        assert_eq!(windows_directory(Some("D:\\WinNT".into())), "D:\\WinNT");
    }
}
