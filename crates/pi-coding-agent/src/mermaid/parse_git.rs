//! Git graph grammar: `commit`, `branch`, `checkout`/`switch`, `merge` and
//! `cherry-pick`, with Mermaid's errors for impossible histories.

use std::collections::HashMap;

use super::labels::clean_label;
use super::parse_flow::{header_kind, statements_of};
use super::strings::{first_word, trim, trim_start};

/// Graphs with more commits or branches than this are refused.
const MAX_COMMITS: usize = 1024;
const MAX_BRANCHES: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CommitKind {
    Normal,
    Reverse,
    Highlight,
    Merge,
    CherryPick,
}

pub(super) struct Commit {
    pub branch: usize,
    /// The first parent, then the merged-in parent.
    pub parents: Vec<usize>,
    pub kind: CommitKind,
    pub id: Option<String>,
    pub tags: Vec<String>,
}

pub(super) struct Branch {
    pub name: String,
    pub order: Option<f64>,
    pub head: Option<usize>,
}

pub(super) struct GitGraph {
    pub commits: Vec<Commit>,
    pub branches: Vec<Branch>,
    /// `BT` draws the history bottom to top.
    pub bottom_up: bool,
}

type Options = Vec<(String, String)>;

/// Parse ` key: value` pairs; values are quoted strings or bare words.
fn parse_options(mut rest: &str) -> Option<Options> {
    let mut out = Vec::new();
    loop {
        rest = trim_start(rest);
        if rest.is_empty() {
            return Some(out);
        }
        let (key, after) = rest.split_once(':')?;
        let key = trim(key);
        if key.is_empty() || key.contains(char::is_whitespace) {
            return None;
        }
        let after = trim_start(after);
        let (value, next) = match after.strip_prefix('"') {
            Some(quoted) => {
                let close = quoted.find('"')?;
                (&quoted[..close], &quoted[close + 1..])
            }
            None => after.split_at(after.find(char::is_whitespace).unwrap_or(after.len())),
        };
        out.push((key.to_ascii_lowercase(), value.to_string()));
        rest = next;
    }
}

/// A branch name, quoted or bare, and the text after it.
fn split_name(rest: &str) -> Option<(String, &str)> {
    let rest = trim_start(rest);
    let (name, after) = match rest.strip_prefix('"') {
        Some(quoted) => {
            let close = quoted.find('"')?;
            (&quoted[..close], &quoted[close + 1..])
        }
        None => rest.split_at(rest.find(char::is_whitespace).unwrap_or(rest.len())),
    };
    let name = clean_label(name);
    if name.is_empty() { None } else { Some((name, after)) }
}

struct Builder {
    graph: GitGraph,
    current: usize,
    by_name: HashMap<String, usize>,
    by_id: HashMap<String, usize>,
}

impl Builder {
    /// Add a commit on the current branch, applying `id`, `tag` and `type` options.
    fn commit(&mut self, mut kind: CommitKind, extra_parent: Option<usize>, options: &Options) -> Option<()> {
        if self.graph.commits.len() >= MAX_COMMITS {
            return None;
        }
        let mut id = None;
        let mut tags = Vec::new();
        for (key, value) in options {
            match key.as_str() {
                "id" => id = Some(value.clone()),
                "tag" => tags.push(value.clone()),
                "msg" => {}
                "type" if kind == CommitKind::Normal => {
                    kind = match value.to_ascii_uppercase().as_str() {
                        "NORMAL" => CommitKind::Normal,
                        "REVERSE" => CommitKind::Reverse,
                        "HIGHLIGHT" => CommitKind::Highlight,
                        _ => return None,
                    }
                }
                "type" if kind == CommitKind::Merge => {}
                _ => return None,
            }
        }
        let index = self.graph.commits.len();
        if let Some(id) = &id {
            if self.by_id.insert(id.clone(), index).is_some() {
                return None;
            }
        }
        let branch = &mut self.graph.branches[self.current];
        let parents = branch.head.into_iter().chain(extra_parent).collect();
        branch.head = Some(index);
        self.graph.commits.push(Commit { branch: self.current, parents, kind, id, tags });
        Some(())
    }

    fn branch(&mut self, rest: &str) -> Option<()> {
        let (name, after) = split_name(rest)?;
        if self.by_name.contains_key(&name) || self.graph.branches.len() >= MAX_BRANCHES {
            return None;
        }
        let mut order = None;
        for (key, value) in parse_options(after)? {
            match key.as_str() {
                "order" => order = Some(value.parse::<f64>().ok().filter(|v| v.is_finite())?),
                _ => return None,
            }
        }
        let head = self.graph.branches[self.current].head;
        self.by_name.insert(name.clone(), self.graph.branches.len());
        self.graph.branches.push(Branch { name, order, head });
        self.current = self.graph.branches.len() - 1;
        Some(())
    }

    fn merge(&mut self, rest: &str) -> Option<()> {
        let (name, after) = split_name(rest)?;
        let source = *self.by_name.get(&name)?;
        let source_head = self.graph.branches[source].head?;
        let head = self.graph.branches[self.current].head;
        if source == self.current || head == Some(source_head) || head.is_none() {
            return None;
        }
        self.commit(CommitKind::Merge, Some(source_head), &parse_options(after)?)
    }

    fn cherry_pick(&mut self, rest: &str) -> Option<()> {
        let mut options = parse_options(rest)?;
        let at = options.iter().position(|(key, _)| key == "id")?;
        let (_, source_id) = options.remove(at);
        let source = *self.by_id.get(&source_id)?;
        if self.graph.commits[source].branch == self.current {
            return None;
        }
        options.retain(|(key, _)| key != "parent");
        self.commit(CommitKind::CherryPick, None, &options)?;
        let picked = self.graph.commits.last_mut()?;
        if picked.id.is_none() {
            picked.id = Some(format!("cherry-pick: {source_id}"));
        }
        Some(())
    }
}

pub(super) fn parse_git(src: &str) -> Option<GitGraph> {
    let statements = statements_of(src);
    if !header_kind(&statements)?.starts_with("gitgraph") {
        return None;
    }
    let bottom_up = statements[0].split(|c: char| !c.is_ascii_alphabetic()).any(|w| w.eq_ignore_ascii_case("bt"));
    let main = Branch { name: "main".into(), order: None, head: None };
    let mut b = Builder {
        graph: GitGraph { commits: Vec::new(), branches: vec![main], bottom_up },
        current: 0,
        by_name: HashMap::from([("main".to_string(), 0)]),
        by_id: HashMap::new(),
    };

    for st in &statements[1..] {
        let keyword = first_word(st);
        let rest = &st[keyword.len()..];
        match keyword.to_ascii_lowercase().as_str() {
            "commit" => b.commit(CommitKind::Normal, None, &parse_options(rest)?)?,
            "branch" => b.branch(rest)?,
            "checkout" | "switch" => {
                let (name, after) = split_name(rest)?;
                if !trim(after).is_empty() {
                    return None;
                }
                b.current = *b.by_name.get(&name)?;
            }
            "merge" => b.merge(rest)?,
            "cherry-pick" => b.cherry_pick(rest)?,
            k if k.starts_with("acctitle") || k.starts_with("accdescr") => {}
            _ => return None,
        }
    }
    if b.graph.commits.is_empty() { None } else { Some(b.graph) }
}
