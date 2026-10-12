//! Semantic versions and conventional commit classification.

use std::fmt;
use std::str::FromStr;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Pre-release label without the leading `-`, for example `rc.1`.
    pub pre: Option<String>,
}

impl FromStr for Version {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim().trim_start_matches('v');
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre.to_string())),
            None => (text, None),
        };
        let core = core.split('+').next().unwrap_or(core);
        let parts: Vec<&str> = core.split('.').collect();
        let number = |part: &str| -> Result<u64, String> {
            if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
                return Err(format!("invalid version number in {text}"));
            }
            part.parse().map_err(|_| format!("invalid version number in {text}"))
        };
        if parts.len() != 3 {
            return Err(format!("expected MAJOR.MINOR.PATCH, got {text}"));
        }
        if pre.as_deref().is_some_and(|pre| {
            pre.is_empty() || !pre.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        }) {
            return Err(format!("invalid pre-release label in {text}"));
        }
        Ok(Self { major: number(parts[0])?, minor: number(parts[1])?, patch: number(parts[2])?, pre })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(formatter, "-{pre}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bump {
    Patch,
    Minor,
    Major,
}

impl FromStr for Bump {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "patch" => Ok(Self::Patch),
            "minor" => Ok(Self::Minor),
            "major" => Ok(Self::Major),
            other => Err(format!("unknown bump {other}; use patch, minor, or major")),
        }
    }
}

impl Version {
    /// Apply `bump` with Cargo's rules: before 1.0 a breaking change bumps the minor
    /// version and every other change bumps the patch version.
    pub fn bump(&self, bump: Bump) -> Self {
        let (major, minor, patch) = match (self.major, bump) {
            (0, Bump::Major) => (0, self.minor + 1, 0),
            (0, _) => (0, self.minor, self.patch + 1),
            (_, Bump::Major) => (self.major + 1, 0, 0),
            (_, Bump::Minor) => (self.major, self.minor + 1, 0),
            (_, Bump::Patch) => (self.major, self.minor, self.patch + 1),
        };
        Self { major, minor, patch, pre: None }
    }
}

/// Changelog sections, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    Breaking,
    Added,
    Changed,
    Fixed,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Self::Breaking => "Breaking Changes",
            Self::Added => "Added",
            Self::Changed => "Changed",
            Self::Fixed => "Fixed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    pub subject: String,
    pub body: String,
}

/// A releasable change parsed from a conventional commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub section: Section,
    pub scope: Option<String>,
    pub description: String,
    pub hash: String,
}

/// Parse `type(scope)!: description`. Returns `None` for commits that do not
/// trigger a release (docs, chore, ci, test, refactor, style, build, release commits).
pub fn classify(commit: &Commit) -> Option<Change> {
    let (head, description) = commit.subject.split_once(':')?;
    let description = description.trim();
    let bang = head.ends_with('!');
    let head = head.trim_end_matches('!');
    let (kind, scope) = match head.split_once('(') {
        Some((kind, rest)) => (kind, rest.strip_suffix(')').map(String::from)),
        None => (head, None),
    };
    let breaking_note =
        commit.body.lines().any(|line| line.starts_with("BREAKING CHANGE:") || line.starts_with("BREAKING-CHANGE:"));
    let breaking = bang || breaking_note;
    let section = match kind.trim().to_ascii_lowercase().as_str() {
        _ if breaking => Section::Breaking,
        "feat" => Section::Added,
        "fix" | "revert" => Section::Fixed,
        "perf" => Section::Changed,
        _ => return None,
    };
    if description.is_empty() {
        return None;
    }
    Some(Change { section, scope, description: description.to_string(), hash: commit.hash.clone() })
}

/// The bump the changes require, if any.
pub fn required_bump(changes: &[Change]) -> Option<Bump> {
    changes
        .iter()
        .map(|change| match change.section {
            Section::Breaking => Bump::Major,
            Section::Added => Bump::Minor,
            Section::Changed | Section::Fixed => Bump::Patch,
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(subject: &str, body: &str) -> Commit {
        Commit { hash: "abc1234".into(), subject: subject.into(), body: body.into() }
    }

    #[test]
    fn parses_and_formats_versions() {
        assert_eq!("v1.2.3".parse::<Version>().unwrap().to_string(), "1.2.3");
        assert_eq!("1.2.3-rc.1".parse::<Version>().unwrap().pre.as_deref(), Some("rc.1"));
        assert!("1.2".parse::<Version>().is_err());
        assert!("01.2.3".parse::<Version>().is_err());
        assert!("1.2.3-".parse::<Version>().is_err());
    }

    #[test]
    fn bumps_follow_cargo_semver() {
        let early: Version = "0.4.2".parse().unwrap();
        assert_eq!(early.bump(Bump::Major).to_string(), "0.5.0");
        assert_eq!(early.bump(Bump::Minor).to_string(), "0.4.3");
        assert_eq!(early.bump(Bump::Patch).to_string(), "0.4.3");
        let stable: Version = "1.4.2".parse().unwrap();
        assert_eq!(stable.bump(Bump::Major).to_string(), "2.0.0");
        assert_eq!(stable.bump(Bump::Minor).to_string(), "1.5.0");
        assert_eq!(stable.bump(Bump::Patch).to_string(), "1.4.3");
    }

    #[test]
    fn classifies_conventional_commits() {
        let feat = classify(&commit("feat(tui): add model picker", "")).unwrap();
        assert_eq!((feat.section, feat.scope.as_deref()), (Section::Added, Some("tui")));
        assert_eq!(classify(&commit("fix: handle empty input", "")).unwrap().section, Section::Fixed);
        assert_eq!(classify(&commit("feat!: drop old flag", "")).unwrap().section, Section::Breaking);
        assert_eq!(classify(&commit("refactor: tidy", "BREAKING CHANGE: renamed")).unwrap().section, Section::Breaking);
        assert_eq!(classify(&commit("docs: readme", "")), None);
        assert_eq!(classify(&commit("chore(release): v1.0.0", "")), None);
        assert_eq!(classify(&commit("Merge branch main", "")), None);
        let changes: Vec<Change> =
            ["fix: a", "feat: b", "docs: c"].iter().filter_map(|subject| classify(&commit(subject, ""))).collect();
        assert_eq!(required_bump(&changes), Some(Bump::Minor));
        assert_eq!(required_bump(&[]), None);
    }
}
