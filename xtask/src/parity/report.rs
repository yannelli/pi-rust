//! The report model (sections, findings, allowlist) and its Markdown rendering.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

/// One finding. `name` is the allowlist key; `group` splits a long list under sub-headings.
#[derive(Clone, Debug, Default)]
pub struct Item {
    pub name: String,
    pub detail: String,
    pub group: Option<String>,
    pub link: Option<String>,
}

impl Item {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), ..Self::default() }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

#[derive(Debug)]
pub enum Status {
    Ran,
    Skipped(String),
}

#[derive(Debug)]
pub struct Section {
    pub id: &'static str,
    pub title: &'static str,
    pub status: Status,
    pub notes: Vec<String>,
    pub missing: Vec<Item>,
    pub extra: Vec<Item>,
    /// A collapsed block: summary line and the lines inside.
    pub collapsed: Option<(String, Vec<String>)>,
    allowed: Vec<(String, String, &'static str)>,
    seen: BTreeSet<String>,
}

impl Section {
    pub fn new(id: &'static str, title: &'static str) -> Self {
        Self {
            id,
            title,
            status: Status::Ran,
            notes: Vec::new(),
            missing: Vec::new(),
            extra: Vec::new(),
            collapsed: None,
            allowed: Vec::new(),
            seen: BTreeSet::new(),
        }
    }

    pub fn skipped(id: &'static str, title: &'static str, reason: impl Into<String>) -> Self {
        Self { status: Status::Skipped(reason.into()), ..Self::new(id, title) }
    }

    /// Distinct names in the missing list, the only-in-pi-rust list, and the allowlisted items.
    pub fn counts(&self) -> (usize, usize, usize) {
        let distinct = |items: &[Item]| items.iter().map(|item| &item.name).collect::<BTreeSet<_>>().len();
        let allowed = self.allowed.iter().map(|(name, _, side)| (name, side)).collect::<BTreeSet<_>>().len();
        (distinct(&self.missing), distinct(&self.extra), allowed)
    }

    /// Move allowlisted items out of both lists.
    pub fn apply(&mut self, allowlist: &Allowlist) {
        for (side, list) in [("missing", &mut self.missing), ("extra", &mut self.extra)] {
            for item in std::mem::take(list) {
                self.seen.insert(item.name.clone());
                match allowlist.reason(self.id, &item.name) {
                    Some(reason) => self.allowed.push((item.name, reason.to_string(), side)),
                    None => list.push(item),
                }
            }
        }
    }

    fn render(&self, number: usize, out: &mut String) {
        out.push_str(&format!("## {number}. {}\n\n", self.title));
        if let Status::Skipped(reason) = &self.status {
            out.push_str(&format!("Skipped: {reason}.\n\n"));
            return;
        }
        for note in &self.notes {
            out.push_str(&format!("{note}\n\n"));
        }
        render_list(out, "Missing in pi-rust", &self.missing);
        render_list(out, "Only in pi-rust", &self.extra);
        if let Some((summary, lines)) = &self.collapsed {
            out.push_str(&format!("<details>\n<summary>{summary}</summary>\n\n{}\n\n</details>\n\n", lines.join("\n")));
        }
        if !self.allowed.is_empty() {
            let lines: Vec<String> =
                self.allowed.iter().map(|(name, reason, _)| format!("- `{name}`: {reason}")).collect();
            let summary = format!("Allowlisted ({})", self.counts().2);
            out.push_str(&format!("<details>\n<summary>{summary}</summary>\n\n{}\n\n</details>\n\n", lines.join("\n")));
        }
    }
}

fn render_list(out: &mut String, title: &str, items: &[Item]) {
    let distinct = items.iter().map(|item| &item.name).collect::<BTreeSet<_>>().len();
    out.push_str(&format!("### {title} ({distinct})\n\n"));
    if items.is_empty() {
        out.push_str("None.\n\n");
        return;
    }
    let groups: BTreeSet<Option<&String>> = items.iter().map(|item| item.group.as_ref()).collect();
    for group in groups {
        if let Some(group) = group {
            out.push_str(&format!("#### {group}\n\n"));
        }
        for item in items.iter().filter(|item| item.group.as_ref() == group) {
            out.push_str(&format!("{}\n", render_item(item)));
        }
        out.push('\n');
    }
}

fn render_item(item: &Item) -> String {
    let name = match &item.link {
        Some(link) => format!("[`{}`]({link})", item.name),
        None => format!("`{}`", item.name),
    };
    let detail = item.detail.replace('<', "&lt;");
    if detail.is_empty() { format!("- {name}") } else { format!("- {name}: {detail}") }
}

/// Known, accepted differences: section id, then item name with a reason.
#[derive(Debug, Default)]
pub struct Allowlist {
    entries: BTreeMap<String, Vec<(String, String)>>,
}

impl Allowlist {
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|error| format!("{}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let Value::Object(sections) = serde_json::from_str(text).map_err(|error| error.to_string())? else {
            return Err("expected an object of sections".into());
        };
        let mut entries = BTreeMap::new();
        for (section, items) in sections {
            let Value::Array(items) = items else { return Err(format!("{section}: expected an array")) };
            let mut parsed = Vec::new();
            for item in items {
                let text = |key: &str| item.get(key).and_then(Value::as_str).map(String::from);
                match (item.as_str(), text("item")) {
                    (Some(name), _) => parsed.push((name.to_string(), String::new())),
                    (None, Some(name)) => parsed.push((name, text("reason").unwrap_or_default())),
                    _ => return Err(format!("{section}: each entry needs an \"item\"")),
                }
            }
            entries.insert(section, parsed);
        }
        Ok(Self { entries })
    }

    pub fn reason(&self, section: &str, name: &str) -> Option<&str> {
        let items = self.entries.get(section)?;
        items.iter().find(|(item, _)| item == name).map(|(_, reason)| reason.as_str())
    }

    /// Entries that match nothing in a section that ran, or name a section that does not exist.
    fn stale(&self, sections: &[Section]) -> Vec<String> {
        let mut stale = Vec::new();
        for (id, items) in &self.entries {
            let section = sections.iter().find(|section| section.id == id);
            for (item, _) in items {
                let matched = match section {
                    None => false,
                    Some(section) => matches!(section.status, Status::Skipped(_)) || section.seen.contains(item),
                };
                if !matched {
                    stale.push(format!("- `{id}`: `{item}`"));
                }
            }
        }
        stale
    }
}

pub struct Report {
    pub header: Vec<String>,
    pub sections: Vec<Section>,
    allowlist: Allowlist,
}

impl Report {
    pub fn new(header: Vec<String>, mut sections: Vec<Section>, allowlist: Allowlist) -> Self {
        for section in &mut sections {
            section.apply(&allowlist);
        }
        Self { header, sections, allowlist }
    }

    /// Findings that are not allowlisted, over all sections that ran.
    pub fn findings(&self) -> usize {
        self.sections.iter().map(|section| section.counts()).map(|(missing, extra, _)| missing + extra).sum()
    }

    pub fn render(&self) -> String {
        let mut out = String::from("# Parity report: pi-rust against upstream pi\n\n");
        for line in &self.header {
            out.push_str(&format!("- {line}\n"));
        }
        out.push_str("\n| Section | Missing in pi-rust | Only in pi-rust | Allowlisted |\n|---|---:|---:|---:|\n");
        let mut totals = (0, 0, 0);
        for (index, section) in self.sections.iter().enumerate() {
            let (number, title) = (index + 1, section.title);
            if matches!(section.status, Status::Skipped(_)) {
                out.push_str(&format!("| {number}. {title} | skipped | skipped | - |\n"));
                continue;
            }
            let (missing, extra, allowed) = section.counts();
            out.push_str(&format!("| {number}. {title} | {missing} | {extra} | {allowed} |\n"));
            totals = (totals.0 + missing, totals.1 + extra, totals.2 + allowed);
        }
        out.push_str(&format!("| **Total** | **{}** | **{}** | **{}** |\n\n", totals.0, totals.1, totals.2));
        for (index, section) in self.sections.iter().enumerate() {
            section.render(index + 1, &mut out);
        }
        let stale = self.allowlist.stale(&self.sections);
        if !stale.is_empty() {
            out.push_str(&format!(
                "## Stale allowlist entries ({})\n\nThese entries match no finding any more.\n\n",
                stale.len()
            ));
            out.push_str(&stale.join("\n"));
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section() -> Section {
        let mut section = Section::new("cliFlags", "CLI flags");
        section.missing = vec![Item::new("--foo"), Item::new("--bar").detail("a <b>")];
        section.extra = vec![Item::new("--baz")];
        section
    }

    #[test]
    fn allowlist_moves_items_and_counts_them() {
        let allowlist =
            Allowlist::parse(r#"{"cliFlags": [{"item": "--foo", "reason": "deliberate"}, "--baz"], "gone": ["x"]}"#)
                .unwrap();
        let mut report = Report::new(vec!["Upstream: x".into()], vec![section()], allowlist);
        assert_eq!(report.sections[0].counts(), (1, 0, 2));
        assert_eq!(report.findings(), 1);
        let text = report.render();
        assert!(text.contains("| 1. CLI flags | 1 | 0 | 2 |"));
        assert!(text.contains("- `--bar`: a &lt;b>"));
        assert!(text.contains("- `--foo`: deliberate"));
        assert!(text.contains("## Stale allowlist entries (1)") && text.contains("- `gone`: `x`"));
        report.sections[0].status = Status::Skipped("no file".into());
        assert!(report.render().contains("Skipped: no file."));
    }

    #[test]
    fn grouped_lists_render_sub_headings() {
        let mut section = Section::new("commits", "Upstream commits");
        section.missing = vec![
            Item { group: Some("ai / fix".into()), link: Some("https://x/1".into()), ..Item::new("abc") },
            Item { group: Some("ai / feat".into()), ..Item::new("def") },
        ];
        let text = Report::new(Vec::new(), vec![section], Allowlist::default()).render();
        assert!(text.find("#### ai / feat").unwrap() < text.find("#### ai / fix").unwrap());
        assert!(text.contains("- [`abc`](https://x/1)"));
    }

    #[test]
    fn allowlist_rejects_bad_shapes() {
        assert!(Allowlist::parse("[]").is_err());
        assert!(Allowlist::parse(r#"{"a": {"b": 1}}"#).is_err());
        assert!(Allowlist::parse(r#"{"a": [{"reason": "x"}]}"#).is_err());
    }
}
