//! Node's `path.posix` and `path.win32` rules that the client applies for the remote system: `resolve`, `join`,
//! `normalize`, `isAbsolute`, and `basename`. Windows support covers drive, rooted, and UNC paths.

/// The path rules of a remote system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathStyle {
    Posix,
    Windows,
}

impl PathStyle {
    /// The rules of a daemon that reported this `os`.
    pub fn of_os(os: &str) -> Self {
        if os == "windows" { Self::Windows } else { Self::Posix }
    }

    pub fn separator(self) -> char {
        match self {
            Self::Posix => '/',
            Self::Windows => '\\',
        }
    }

    fn is_separator(self, c: char) -> bool {
        c == '/' || (self == Self::Windows && c == '\\')
    }

    /// The root of `path` (`/`, `C:\`, `C:`, `\`, `\\server\share\`) and the rest.
    fn split_root(self, path: &str) -> (String, &str) {
        if self == Self::Posix {
            return match path.strip_prefix('/') {
                Some(_) => ("/".into(), path.trim_start_matches('/')),
                None => (String::new(), path),
            };
        }
        let chars: Vec<char> = path.chars().take(3).collect();
        if chars.len() >= 2 && chars[0].is_ascii_alphabetic() && chars[1] == ':' {
            let drive = &path[..2];
            if chars.len() == 3 && self.is_separator(chars[2]) {
                return (format!("{drive}\\"), path[3..].trim_start_matches(['/', '\\']));
            }
            return (drive.to_string(), &path[2..]);
        }
        if chars.len() >= 2 && self.is_separator(chars[0]) && self.is_separator(chars[1]) {
            let rest = &path[2..];
            let parts: Vec<&str> = rest.splitn(3, ['/', '\\']).collect();
            if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                let tail = parts.get(2).copied().unwrap_or("");
                return (format!("\\\\{}\\{}\\", parts[0], parts[1]), tail.trim_start_matches(['/', '\\']));
            }
        }
        if chars.first().is_some_and(|c| self.is_separator(*c)) {
            return ("\\".into(), path.trim_start_matches(['/', '\\']));
        }
        (String::new(), path)
    }

    pub fn is_absolute(self, path: &str) -> bool {
        let (root, _) = self.split_root(path);
        root.ends_with(['/', '\\'])
    }

    /// Node's `normalize`: `.` dropped, `..` applied, repeated separators collapsed, a trailing separator kept.
    pub fn normalize(self, path: &str) -> String {
        if path.is_empty() {
            return ".".into();
        }
        let (root, rest) = self.split_root(path);
        let absolute = root.ends_with(['/', '\\']);
        let trailing = rest.ends_with(|c| self.is_separator(c));
        let mut parts: Vec<&str> = Vec::new();
        for part in rest.split(|c| self.is_separator(c)) {
            match part {
                "" | "." => {}
                ".." => {
                    if parts.last().is_some_and(|last| *last != "..") {
                        parts.pop();
                    } else if !absolute {
                        parts.push("..");
                    }
                }
                part => parts.push(part),
            }
        }
        let separator = self.separator().to_string();
        let mut tail = parts.join(&separator);
        if tail.is_empty() && !absolute {
            tail = ".".into();
        }
        if trailing && !tail.is_empty() {
            tail.push(self.separator());
        }
        format!("{root}{tail}")
    }

    /// Node's `join`: the non-empty parts joined and normalized; `.` for none.
    pub fn join(self, parts: &[&str]) -> String {
        let parts: Vec<&str> = parts.iter().copied().filter(|part| !part.is_empty()).collect();
        if parts.is_empty() {
            return ".".into();
        }
        self.normalize(&parts.join(&self.separator().to_string()))
    }

    /// Node's `resolve(base, path)` for an absolute `base`, without a trailing separator.
    pub fn resolve(self, base: &str, path: &str) -> String {
        let joined = if self.is_absolute(path) {
            let (root, rest) = self.split_root(path);
            if self == Self::Windows && root == "\\" {
                // Rooted without a drive: the root of the base's drive.
                let (base_root, _) = self.split_root(base);
                format!("{base_root}{rest}")
            } else {
                path.to_string()
            }
        } else if self == Self::Windows && self.split_root(path).0.len() == 2 {
            // `C:foo` on the base's drive.
            format!("{base}\\{}", &path[2..])
        } else {
            format!("{base}{}{path}", self.separator())
        };
        let normalized = self.normalize(&joined);
        let (root, _) = self.split_root(&normalized);
        if normalized.len() > root.len() { normalized.trim_end_matches(['/', '\\']).to_string() } else { normalized }
    }

    /// The last component, without trailing separators.
    pub fn basename(self, path: &str) -> String {
        let (_, rest) = self.split_root(path);
        let trimmed = rest.trim_end_matches(|c| self.is_separator(c));
        trimmed.rsplit(|c| self.is_separator(c)).next().unwrap_or("").to_string()
    }
}

/// Node's `fileURLToPath` for the remote system; `None` for a URL Node refuses, which then stays an ordinary path.
pub(crate) fn file_url_to_path(url: &str, style: PathStyle) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.scheme() != "file" {
        return None;
    }
    let host = parsed.host_str().unwrap_or("");
    let raw = parsed.path();
    let lower = raw.to_ascii_lowercase();
    if lower.contains("%2f") || (style == PathStyle::Windows && lower.contains("%5c")) {
        return None;
    }
    let decoded = percent_encoding::percent_decode_str(raw).decode_utf8().ok()?.into_owned();
    match style {
        PathStyle::Posix => (host.is_empty() || host == "localhost").then_some(decoded),
        PathStyle::Windows => {
            let path = decoded.replace('/', "\\");
            if !host.is_empty() && host != "localhost" {
                return Some(format!("\\\\{host}{path}"));
            }
            let bytes = path.as_bytes();
            let drive = bytes.len() >= 3 && bytes[1].is_ascii_alphabetic() && bytes[2] == b':';
            drive.then(|| path[1..].to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PathStyle::{Posix, Windows};

    #[test]
    fn posix_rules() {
        assert_eq!(Posix.resolve("/a/b", "../c/./d/"), "/a/c/d");
        assert_eq!(Posix.resolve("/a", "/x//y"), "/x/y");
        assert_eq!(Posix.resolve("/a", ".."), "/");
        assert_eq!(Posix.join(&["/tmp", "../x"]), "/x");
        assert_eq!(Posix.join(&["a", "", "b/"]), "a/b/");
        assert_eq!(Posix.basename("/a/b.txt/"), "b.txt");
    }

    #[test]
    fn windows_rules() {
        assert_eq!(Windows.resolve("C:\\work", "a/b"), "C:\\work\\a\\b");
        assert_eq!(Windows.resolve("C:\\work", "\\x"), "C:\\x");
        assert_eq!(Windows.resolve("C:\\work", "D:\\y\\..\\z"), "D:\\z");
        assert_eq!(Windows.resolve("C:\\work", "C:sub"), "C:\\work\\sub");
        assert_eq!(Windows.resolve("C:\\work", "\\\\srv\\share\\f"), "\\\\srv\\share\\f");
        assert_eq!(Windows.join(&["C:\\t", "tmp-"]), "C:\\t\\tmp-");
        assert_eq!(Windows.basename("C:\\a\\b"), "b");
        assert!(!Windows.is_absolute("C:x"));
    }
}
