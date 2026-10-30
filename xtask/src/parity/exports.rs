//! The runtime exports of one TypeScript or JavaScript module, read from its text. Type-only
//! exports (`export type`, `export interface`, `export declare`, `type` list entries) are left out.

use std::collections::{BTreeMap, BTreeSet};

use super::exports_lex::{braces, skip_code, skip_space};
use super::scan::{Lang, is_ident_byte, literal_at, matching_close, skip_lexeme};

/// Where an exported name gets its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Declared in this module.
    Value,
    /// `imported` of module `spec`; `*` is the module namespace.
    From { spec: String, imported: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub source: Source,
    /// The export statement mentions `ERR_PI_UNSUPPORTED` or an `unsupportedFunction`/`unsupportedClass` stub.
    pub unsupported: bool,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub entries: Vec<Entry>,
    /// `export * from "<spec>"`.
    pub stars: Vec<String>,
}

type Link = Option<(String, String)>;

const STUB_MARKERS: &[&str] = &["ERR_PI_UNSUPPORTED", "unsupportedFunction(", "unsupportedClass("];

#[derive(Default)]
struct State {
    entries: Vec<(usize, String, Link)>,
    locals: Vec<(usize, String, String)>,
    stars: Vec<String>,
    imports: BTreeMap<String, Link>,
    values: BTreeSet<String>,
    types: BTreeSet<String>,
    starts: Vec<usize>,
}

/// The identifier after optional space at `i`, and the index past it.
fn word(src: &str, i: usize) -> (&str, usize) {
    let start = skip_space(src, i);
    let len = src.as_bytes()[start..].iter().take_while(|&&byte| is_ident_byte(byte)).count();
    (&src[start..start + len], start + len)
}

fn punct(src: &str, i: usize) -> (Option<u8>, usize) {
    let at = skip_space(src, i);
    (src.as_bytes().get(at).copied(), at)
}

/// The module specifier of a `from "<spec>"` clause at `i`.
fn from_clause(src: &str, i: usize) -> Option<(String, usize)> {
    let (keyword, after) = word(src, i);
    if keyword != "from" {
        return None;
    }
    let at = skip_space(src, after);
    let spec = literal_at(src, at, Lang::Ts)?;
    Some((spec.to_string(), skip_lexeme(src, at, Lang::Ts)?))
}

fn unquote(text: &str) -> &str {
    text.trim().trim_matches(|c| c == '"' || c == '\'')
}

fn is_type_entry(item: &str) -> bool {
    item.strip_prefix("type").is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

/// Entries of a `{ a, b as c, type d }` list as `(imported, local or exported)`, type entries left out.
fn list(body: &str) -> Vec<(String, String)> {
    let items = body.split(',').map(str::trim).filter(|item| !item.is_empty() && !is_type_entry(item));
    let pairs = items.map(|item| item.split_once(" as ").unwrap_or((item, item)));
    pairs.map(|(from, to)| (unquote(from).to_string(), unquote(to).to_string())).collect()
}

impl State {
    fn value(&mut self, at: usize, name: &str) {
        if !name.is_empty() {
            self.values.insert(name.to_string());
            self.entries.push((at, name.to_string(), None));
        }
    }

    fn export(&mut self, src: &str, at: usize, i: usize) -> usize {
        self.starts.push(at);
        let (keyword, after) = word(src, i);
        match keyword {
            "default" => self.value(at, "default"),
            "declare" | "interface" => {}
            "type" => {
                let (name, end) = word(src, after);
                self.types.insert(name.to_string());
                return end;
            }
            "async" | "function" | "abstract" | "class" | "enum" | "namespace" | "module" | "import" => {
                let mut end = after;
                let name = loop {
                    let (_, at) = punct(src, end);
                    let at = if src.as_bytes().get(at) == Some(&b'*') { at + 1 } else { at };
                    let (name, next) = word(src, at);
                    end = next;
                    if !matches!(name, "function" | "class" | "enum") {
                        break name;
                    }
                };
                self.value(at, name);
                return end;
            }
            "const" | "let" | "var" => {
                let (name, end) = word(src, after);
                if name == "enum" {
                    let (name, end) = word(src, end);
                    self.value(at, name);
                    return end;
                }
                let (open, open_at) = punct(src, after);
                if matches!(open, Some(b'{' | b'[')) {
                    let close = matching_close(src, open_at, Lang::Ts).unwrap_or(src.len() - 1);
                    for part in src[open_at + 1..close].split(',') {
                        let part = part.rsplit(':').next().unwrap_or(part);
                        self.value(at, part.split('=').next().unwrap_or("").trim().trim_start_matches("..."));
                    }
                    return close + 1;
                }
                self.value(at, name);
                return end;
            }
            "" => return self.export_clause(src, at, after),
            _ => {}
        }
        after
    }

    fn export_clause(&mut self, src: &str, at: usize, i: usize) -> usize {
        match punct(src, i) {
            (Some(b'*'), star) => {
                let (keyword, after) = word(src, star + 1);
                let (name, after) = if keyword == "as" { word(src, after) } else { ("", star + 1) };
                let Some((spec, end)) = from_clause(src, after) else { return after };
                match name {
                    "" => self.stars.push(spec),
                    name => self.entries.push((at, name.to_string(), Some((spec, "*".into())))),
                }
                end
            }
            (Some(b'{'), open) => {
                let Some((body, end)) = braces(src, open) else { return open + 1 };
                let from = from_clause(src, end);
                for (imported, exported) in list(&body) {
                    match &from {
                        Some((spec, _)) => self.entries.push((at, exported, Some((spec.clone(), imported)))),
                        None => self.locals.push((at, exported, imported)),
                    }
                }
                from.map_or(end, |(_, end)| end)
            }
            (_, at) => at,
        }
    }

    fn import(&mut self, src: &str, i: usize) -> usize {
        let (first, mut at) = word(src, i);
        let type_only = first == "type" && punct(src, at).0 != Some(b',') && word(src, at).0 != "from";
        let mut bindings = Vec::new();
        if !first.is_empty() && !type_only {
            bindings.push(("default".to_string(), first.to_string()));
        }
        loop {
            match punct(src, at) {
                (Some(b','), comma) => at = comma + 1,
                (Some(b'*'), star) => {
                    let (name, end) = word(src, word(src, star + 1).1);
                    bindings.push(("*".into(), name.to_string()));
                    at = end;
                }
                (Some(b'{'), open) => {
                    let Some((body, end)) = braces(src, open) else { return open + 1 };
                    for item in body.split(',').map(str::trim).filter(|item| is_type_entry(item)) {
                        self.imports.insert(item.rsplit(' ').next().unwrap_or(item).to_string(), None);
                    }
                    bindings.extend(list(&body));
                    at = end;
                }
                _ => break,
            }
        }
        let Some((spec, end)) = from_clause(src, at) else { return at };
        for (imported, local) in bindings {
            self.imports.insert(local, (!type_only).then(|| (spec.clone(), imported)));
        }
        end
    }

    fn declaration(&mut self, keyword: &str, src: &str, i: usize) -> usize {
        let (name, end) = word(src, i);
        let set = if matches!(keyword, "type" | "interface") { &mut self.types } else { &mut self.values };
        if name.is_empty() {
            return i;
        }
        set.insert(name.to_string());
        end
    }

    fn finish(mut self, src: &str) -> Scan {
        for (at, exported, local) in std::mem::take(&mut self.locals) {
            match self.imports.get(&local) {
                Some(Some(link)) => self.entries.push((at, exported, Some(link.clone()))),
                Some(None) => {}
                None if self.types.contains(&local) && !self.values.contains(&local) => {}
                None => self.entries.push((at, exported, None)),
            }
        }
        let unsupported = |at: usize| {
            let end = self.starts.iter().copied().find(|&start| start > at).unwrap_or(src.len());
            let text = &src[at..end];
            STUB_MARKERS.iter().any(|marker| text.contains(marker))
        };
        let entries = self.entries.iter().map(|(at, name, link)| Entry {
            name: name.clone(),
            source: link.clone().map_or(Source::Value, |(spec, imported)| Source::From { spec, imported }),
            unsupported: unsupported(*at),
        });
        Scan { entries: entries.collect(), stars: self.stars }
    }
}

/// The export statements at the top level of `src`.
pub fn scan(src: &str) -> Scan {
    let b = src.as_bytes();
    let (mut state, mut depth, mut i) = (State::default(), 0usize, 0);
    while i < b.len() {
        if let Some(end) = skip_code(src, i) {
            i = end;
            continue;
        }
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            byte if depth == 0 && is_ident_byte(byte) && (i == 0 || !is_ident_byte(b[i - 1]) && b[i - 1] != b'.') => {
                let (keyword, end) = word(src, i);
                i = match keyword {
                    "export" => state.export(src, i, end),
                    "import" if !matches!(punct(src, end).0, Some(b'(' | b'.')) => state.import(src, end),
                    "type" | "interface" | "function" | "class" | "const" | "let" | "var" | "enum" | "namespace" => {
                        state.declaration(keyword, src, end)
                    }
                    _ => end,
                };
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    state.finish(src)
}
