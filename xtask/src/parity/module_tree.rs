//! A tree of module sources (on disk or in memory) and the runtime exports of a module in it,
//! following `export *` and named re-exports through relative paths and bare specifiers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use super::exports::{Source, scan};

/// The file that defines an export, and whether its statement throws `ERR_PI_UNSUPPORTED`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    pub file: String,
    pub unsupported: bool,
}

pub type Exports = BTreeMap<String, Origin>;

/// Module sources keyed by `/`-separated paths. Bare specifiers map through `bare` patterns,
/// where one `*` in a pattern carries over into its target.
#[derive(Default)]
pub struct Tree {
    pub files: BTreeMap<String, String>,
    pub disk: Option<PathBuf>,
    pub bare: Vec<(String, String)>,
}

const SUFFIXES: &[&str] = &["", ".ts", ".js", ".mjs", "/index.ts", "/index.js", "/index.mjs"];

fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => _ = parts.pop(),
            part => parts.push(part),
        }
    }
    parts.join("/")
}

fn wildcard(pattern: &str, spec: &str) -> Option<String> {
    match pattern.split_once('*') {
        None => (pattern == spec).then(String::new),
        Some((head, tail)) => {
            let rest = spec.strip_prefix(head)?.strip_suffix(tail)?;
            (!rest.is_empty()).then(|| rest.to_string())
        }
    }
}

impl Tree {
    pub fn read(&self, key: &str) -> Option<String> {
        if let Some(text) = self.files.get(key) {
            return Some(text.clone());
        }
        std::fs::read_to_string(self.disk.as_ref()?.join(key)).ok()
    }

    fn exists(&self, key: &str) -> bool {
        self.files.contains_key(key) || self.disk.as_ref().is_some_and(|root| root.join(key).is_file())
    }

    fn file(&self, base: &str) -> Option<String> {
        let ts = base.strip_suffix(".js").map(|stem| format!("{stem}.ts"));
        let candidates = SUFFIXES.iter().map(|suffix| format!("{base}{suffix}")).chain(ts);
        candidates.filter(|key| !key.ends_with(".d.ts")).find(|key| self.exists(key))
    }

    /// The key that `spec`, imported from the module `from`, names.
    pub fn resolve(&self, from: &str, spec: &str) -> Option<String> {
        if let Some(absolute) = spec.strip_prefix('/') {
            return self.file(&normalize(absolute));
        }
        if spec.starts_with('.') {
            let dir = from.rsplit_once('/').map_or("", |(dir, _)| dir);
            return self.file(&normalize(&format!("{dir}/{spec}")));
        }
        self.bare.iter().find_map(|(pattern, target)| {
            let star = wildcard(pattern, spec)?;
            self.file(&target.replacen('*', &star, 1))
        })
    }
}

/// Resolves modules of one tree, once each, and keeps the specifiers it could not follow.
pub struct Walker<'a> {
    tree: &'a Tree,
    memo: HashMap<String, Exports>,
    visiting: HashSet<String>,
    pub unresolved: BTreeSet<String>,
}

impl<'a> Walker<'a> {
    pub fn new(tree: &'a Tree) -> Self {
        Self { tree, memo: HashMap::new(), visiting: HashSet::new(), unresolved: BTreeSet::new() }
    }

    pub fn tree(&self) -> &Tree {
        self.tree
    }

    /// Runtime exports of `key`. A named re-export from a module that cannot be found counts as present.
    pub fn exports(&mut self, key: &str) -> Exports {
        if let Some(done) = self.memo.get(key) {
            return done.clone();
        }
        if !self.visiting.insert(key.to_string()) {
            return Exports::new();
        }
        let scanned = scan(&self.tree.read(key).unwrap_or_default());
        let mut found = Exports::new();
        for entry in scanned.entries {
            let here = Origin { file: key.to_string(), unsupported: entry.unsupported };
            let origin = match entry.source {
                Source::Value => Some(here),
                Source::From { imported, .. } if imported == "*" => Some(here),
                Source::From { spec, imported } => match self.tree.resolve(key, &spec) {
                    Some(target) => self.exports(&target).remove(&imported).map(|mut origin| {
                        origin.unsupported |= here.unsupported;
                        origin
                    }),
                    None => {
                        self.unresolved.insert(format!("{spec} (from {key})"));
                        Some(here)
                    }
                },
            };
            if let Some(origin) = origin {
                found.insert(entry.name, origin);
            }
        }
        for spec in scanned.stars {
            let Some(target) = self.tree.resolve(key, &spec) else {
                self.unresolved.insert(format!("export * from {spec} (in {key})"));
                continue;
            };
            for (name, origin) in self.exports(&target) {
                if name != "default" {
                    found.entry(name).or_insert(origin);
                }
            }
        }
        self.visiting.remove(key);
        self.memo.insert(key.to_string(), found.clone());
        found
    }
}
