//! Module specifier resolution with Node and jiti semantics. Order: `node:` and bare built-in
//! names, virtual modules, `#imports`, relative and absolute paths (with extension and `index`
//! probing), then packages found by walking up `node_modules`.

mod package;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use package::PackageJson;

use crate::error::JsError;

/// Every Node built-in module name, without the `node:` prefix.
pub const NODE_BUILTINS: &[&str] = &[
    "assert",
    "assert/strict",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "dns/promises",
    "domain",
    "events",
    "fs",
    "fs/promises",
    "http",
    "http2",
    "https",
    "inspector",
    "inspector/promises",
    "module",
    "net",
    "os",
    "path",
    "path/posix",
    "path/win32",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "readline/promises",
    "repl",
    "stream",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "string_decoder",
    "sys",
    "timers",
    "timers/promises",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "util/types",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
    "test",
    "test/reporters",
    "sqlite",
    "sea",
];

/// Built-in modules that load only with the `node:` prefix, as in Node.
pub const PREFIX_ONLY: &[&str] = &["sea", "sqlite", "test", "test/reporters"];

/// Whether the import is an ES `import` or a CommonJS `require`; this picks `exports` conditions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveKind {
    Import,
    Require,
}

impl ResolveKind {
    fn conditions(self) -> &'static [&'static str] {
        match self {
            Self::Import => &["node", "import"],
            Self::Require => &["node", "require"],
        }
    }
}

/// Where a specifier leads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// A Node built-in, named with its `node:` prefix, or the `pi:host` module.
    Builtin(String),
    /// A module registered from Rust, by canonical name.
    Virtual(String),
    File(PathBuf),
}

const JS_FIRST: &[&str] = &["js", "mjs", "cjs", "ts", "tsx", "mts", "cts", "jsx", "json"];
const TS_FIRST: &[&str] = &["ts", "tsx", "mts", "cts", "js", "mjs", "cjs", "jsx", "json"];

/// Resolves specifiers; `package.json` reads are cached for the resolver's lifetime.
#[derive(Clone, Debug, Default)]
pub struct ModuleResolver {
    packages: Arc<Mutex<HashMap<PathBuf, Option<Arc<PackageJson>>>>>,
}

impl ModuleResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve `specifier` imported by the file `from` (or from `base_dir` for entry points).
    /// `virtual_name` maps a specifier to a registered virtual module's canonical name.
    pub fn resolve(
        &self,
        specifier: &str,
        from: Option<&Path>,
        base_dir: &Path,
        kind: ResolveKind,
        virtual_name: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Resolved, JsError> {
        if let Some(name) = specifier.strip_prefix("node:") {
            if NODE_BUILTINS.contains(&name) {
                return Ok(Resolved::Builtin(format!("node:{name}")));
            }
            return Err(
                JsError::new(format!("No such built-in module: {specifier}")).with_code("ERR_UNKNOWN_BUILTIN_MODULE")
            );
        }
        if NODE_BUILTINS.contains(&specifier) && !PREFIX_ONLY.contains(&specifier) {
            return Ok(Resolved::Builtin(format!("node:{specifier}")));
        }
        if specifier == "pi:host" {
            return Ok(Resolved::Builtin(specifier.to_string()));
        }
        if let Some(name) = virtual_name(specifier) {
            return Ok(Resolved::Virtual(name));
        }
        let dir = from.and_then(Path::parent).unwrap_or(base_dir);
        let prefer_ts = from.is_some_and(crate::transpile::needs_transform);
        let found = if specifier.starts_with('#') {
            self.resolve_imports(specifier, dir, kind, prefer_ts, virtual_name)?
        } else if let Some(path) = file_url_path(specifier) {
            resolve_path(&path, prefer_ts)
        } else if is_path_specifier(specifier) {
            resolve_path(&normalize(&dir.join(specifier)), prefer_ts)
        } else {
            self.resolve_package(specifier, dir, kind, prefer_ts)?
        };
        match found {
            Some(path) => Ok(Resolved::File(canonical(&path))),
            None => Err(not_found(specifier, from.unwrap_or(base_dir), kind)),
        }
    }

    fn package(&self, dir: &Path) -> Option<Arc<PackageJson>> {
        let mut cache = self.packages.lock().unwrap_or_else(|poison| poison.into_inner());
        cache.entry(dir.to_path_buf()).or_insert_with(|| PackageJson::read(dir).map(Arc::new)).clone()
    }

    /// The closest `package.json` at or above `dir`.
    pub fn nearest_package(&self, dir: &Path) -> Option<Arc<PackageJson>> {
        dir.ancestors().find_map(|ancestor| self.package(ancestor))
    }

    fn resolve_imports(
        &self,
        specifier: &str,
        dir: &Path,
        kind: ResolveKind,
        prefer_ts: bool,
        virtual_name: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<PathBuf>, JsError> {
        let Some(package) = self.nearest_package(dir) else { return Ok(None) };
        let Some(target) = package.resolve_import(specifier, kind.conditions())? else { return Ok(None) };
        if target.starts_with("./") {
            return Ok(resolve_path(&normalize(&package.dir.join(&target)), prefer_ts));
        }
        match self.resolve(&target, None, &package.dir, kind, virtual_name)? {
            Resolved::File(path) => Ok(Some(path)),
            _ => Err(JsError::new(format!("#imports target {target} must be a file"))
                .with_code("ERR_INVALID_PACKAGE_TARGET")),
        }
    }

    fn resolve_package(
        &self,
        specifier: &str,
        dir: &Path,
        kind: ResolveKind,
        prefer_ts: bool,
    ) -> Result<Option<PathBuf>, JsError> {
        let (name, subpath) = split_package(specifier)
            .ok_or_else(|| JsError::type_error(format!("Invalid module specifier '{specifier}'")))?;
        // A package may import itself by name through its own exports.
        if let Some(own) = self.nearest_package(dir)
            && own.name.as_deref() == Some(name)
            && own.exports.is_some()
        {
            return self.package_entry(&own, &subpath, kind, prefer_ts);
        }
        for ancestor in dir.ancestors() {
            if ancestor.file_name().is_some_and(|last| last == "node_modules") {
                continue;
            }
            let package_dir = ancestor.join("node_modules").join(name);
            if !package_dir.is_dir() {
                continue;
            }
            return match self.package(&package_dir) {
                Some(package) => self.package_entry(&package, &subpath, kind, prefer_ts),
                None => Ok(resolve_path(&normalize(&package_dir.join(&subpath)), prefer_ts)),
            };
        }
        Ok(None)
    }

    fn package_entry(
        &self,
        package: &PackageJson,
        subpath: &str,
        kind: ResolveKind,
        prefer_ts: bool,
    ) -> Result<Option<PathBuf>, JsError> {
        if let Some(target) = package.resolve_export(subpath, kind.conditions())? {
            let path = normalize(&package.dir.join(target));
            return Ok(path.is_file().then_some(path));
        }
        if subpath != "." {
            return Ok(resolve_path(&normalize(&package.dir.join(subpath)), prefer_ts));
        }
        let fields = match kind {
            ResolveKind::Import => [package.module.as_deref(), package.main.as_deref()],
            ResolveKind::Require => [package.main.as_deref(), None],
        };
        for entry in fields.into_iter().flatten() {
            if let Some(found) = resolve_path(&normalize(&package.dir.join(entry)), prefer_ts) {
                return Ok(Some(found));
            }
        }
        Ok(resolve_index(&package.dir, prefer_ts))
    }
}

/// Probe a file path: exact, `.js` written for a `.ts` source, added extensions, then a directory.
pub fn resolve_path(path: &Path, prefer_ts: bool) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    let extensions = if prefer_ts { TS_FIRST } else { JS_FIRST };
    let text = path.to_string_lossy();
    for (written, sources) in [(".js", &["ts", "tsx"][..]), (".mjs", &["mts"]), (".cjs", &["cts"]), (".jsx", &["tsx"])]
    {
        if let Some(stem) = text.strip_suffix(written)
            && let Some(found) = sources.iter().map(|ext| PathBuf::from(format!("{stem}.{ext}"))).find(|p| p.is_file())
        {
            return Some(found);
        }
    }
    if let Some(found) = with_extension(path, extensions) {
        return Some(found);
    }
    if !path.is_dir() {
        return None;
    }
    if let Some(main) = PackageJson::read(path).and_then(|package| package.main) {
        let main_path = normalize(&path.join(main));
        if main_path.is_file() {
            return Some(main_path);
        }
        if let Some(found) = with_extension(&main_path, extensions) {
            return Some(found);
        }
        if let Some(found) = main_path.is_dir().then(|| resolve_index(&main_path, prefer_ts)).flatten() {
            return Some(found);
        }
    }
    resolve_index(path, prefer_ts)
}

fn with_extension(path: &Path, extensions: &[&str]) -> Option<PathBuf> {
    let text = path.to_string_lossy();
    extensions.iter().map(|ext| PathBuf::from(format!("{text}.{ext}"))).find(|p| p.is_file())
}

fn resolve_index(dir: &Path, prefer_ts: bool) -> Option<PathBuf> {
    let extensions = if prefer_ts { TS_FIRST } else { JS_FIRST };
    extensions.iter().map(|ext| dir.join(format!("index.{ext}"))).find(|p| p.is_file())
}

fn is_path_specifier(specifier: &str) -> bool {
    specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier == "."
        || specifier == ".."
        || Path::new(specifier).is_absolute()
        || (cfg!(windows) && (specifier.starts_with(".\\") || specifier.starts_with("..\\")))
}

fn file_url_path(specifier: &str) -> Option<PathBuf> {
    url::Url::parse(specifier).ok().filter(|url| url.scheme() == "file")?.to_file_path().ok()
}

/// `@scope/name/sub` to (`@scope/name`, `./sub`); `name` to (`name`, `.`).
fn split_package(specifier: &str) -> Option<(&str, String)> {
    let split_at = if specifier.starts_with('@') {
        let slash = specifier.find('/')?;
        specifier[slash + 1..].find('/').map(|index| slash + 1 + index)
    } else {
        specifier.find('/')
    };
    let (name, rest) = match split_at {
        Some(index) => (&specifier[..index], &specifier[index..]),
        None => (specifier, ""),
    };
    if name.is_empty() || name.ends_with('/') {
        return None;
    }
    Some((name, format!(".{rest}")))
}

/// Lexically normalize `.` and `..` components.
pub fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    result.push("..");
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// The real path without Windows verbatim prefixes, falling back to the given path.
pub fn canonical(path: &Path) -> PathBuf {
    let Ok(real) = std::fs::canonicalize(path) else { return path.to_path_buf() };
    let text = real.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => real,
    }
}

fn not_found(specifier: &str, from: &Path, kind: ResolveKind) -> JsError {
    let code = match kind {
        ResolveKind::Import => "ERR_MODULE_NOT_FOUND",
        ResolveKind::Require => "MODULE_NOT_FOUND",
    };
    JsError::new(format!("Cannot find module '{specifier}' imported from {}", from.display())).with_code(code)
}

#[cfg(test)]
mod tests;
