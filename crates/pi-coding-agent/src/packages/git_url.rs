//! Git source URLs, ported from upstream `utils/git.ts` together with the host rules of
//! `hosted-git-info` that it relies on (GitHub, GitLab, Bitbucket, and SourceHut).

use reqwest::Url;

/// A parsed git package source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSource {
    /// Clone URL without the ref suffix.
    pub repo: String,
    /// Host name such as `github.com`.
    pub host: String,
    /// Repository path such as `user/repo`.
    pub path: String,
    /// Branch, tag, or commit after `@` (or `#` for hosted URLs).
    pub git_ref: Option<String>,
    /// A ref was given, so updates keep the checkout on it.
    pub pinned: bool,
}

fn scp_like(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("git@")?;
    let (host, path) = rest.split_once(':')?;
    (!host.is_empty() && !path.is_empty()).then_some((host, path))
}

fn split_at_ref(path: &str) -> Option<(&str, &str)> {
    let (repo, git_ref) = path.split_once('@')?;
    (!repo.is_empty() && !git_ref.is_empty()).then_some((repo, git_ref))
}

/// Split a trailing `@ref` from the repository part of `url`.
fn split_ref(url: &str) -> (String, Option<String>) {
    let unchanged = || (url.to_string(), None);
    if let Some((host, path)) = scp_like(url) {
        return match split_at_ref(path) {
            Some((repo, git_ref)) => (format!("git@{host}:{repo}"), Some(git_ref.into())),
            None => unchanged(),
        };
    }
    if url.contains("://") {
        let Ok(mut parsed) = Url::parse(url) else { return unchanged() };
        let path = parsed.path().trim_start_matches('/').to_string();
        let Some((repo, git_ref)) = split_at_ref(&path) else { return unchanged() };
        parsed.set_path(&format!("/{repo}"));
        let text = parsed.to_string();
        return (text.strip_suffix('/').unwrap_or(&text).to_string(), Some(git_ref.into()));
    }
    let Some((host, path)) = url.split_once('/') else { return unchanged() };
    match split_at_ref(path) {
        Some((repo, git_ref)) => (format!("{host}/{repo}"), Some(git_ref.into())),
        None => unchanged(),
    }
}

/// `decodeURIComponent`: `None` for a malformed escape or invalid UTF-8.
pub(crate) fn decode_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn has_unsafe_install_part(value: &str, allow_slash: bool) -> bool {
    let Some(decoded) = decode_component(value) else { return true };
    [value, decoded.as_str()].iter().any(|candidate| {
        candidate.contains('\0')
            || candidate.contains('\\')
            || candidate.starts_with('/')
            || (!allow_slash && candidate.contains('/'))
            || candidate.split('/').any(|segment| segment == "..")
    })
}

fn build_git_source(repo: String, host: &str, path: &str, git_ref: Option<String>) -> Option<GitSource> {
    if path.starts_with('/') {
        return None;
    }
    let normalized = path.strip_suffix(".git").unwrap_or(path).trim_start_matches('/');
    if host.is_empty() || normalized.is_empty() || normalized.split('/').count() < 2 {
        return None;
    }
    if has_unsafe_install_part(host, false) || has_unsafe_install_part(normalized, true) {
        return None;
    }
    let pinned = git_ref.is_some();
    Some(GitSource { repo, host: host.to_string(), path: normalized.to_string(), git_ref, pinned })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Host {
    GitHub,
    GitLab,
    Bitbucket,
    SourceHut,
}

impl Host {
    fn domain(self) -> &'static str {
        match self {
            Self::GitHub => "github.com",
            Self::GitLab => "gitlab.com",
            Self::Bitbucket => "bitbucket.org",
            Self::SourceHut => "git.sr.ht",
        }
    }

    fn from_domain(domain: &str) -> Option<Self> {
        let domain = domain.strip_prefix("www.").unwrap_or(domain);
        [Self::GitHub, Self::GitLab, Self::Bitbucket, Self::SourceHut].into_iter().find(|host| host.domain() == domain)
    }
}

struct Hosted {
    host: Host,
    user: String,
    project: String,
    committish: Option<String>,
    /// Parsed from a `github:user/repo` or `user/repo` shortcut.
    shortcut: bool,
}

fn strip_git(project: &str) -> &str {
    project.strip_suffix(".git").unwrap_or(project)
}

/// The `extract` rules of `hosted-git-info` for each host.
fn extract(host: Host, pathname: &str, fragment: &str) -> Option<(String, String, Option<String>)> {
    let fragment = (!fragment.is_empty()).then(|| fragment.to_string());
    let segments: Vec<&str> = pathname.split('/').collect();
    let at = |index: usize| segments.get(index).copied().unwrap_or_default();
    let (user, project, committish) = match host {
        Host::GitHub => {
            let kind = at(3);
            if !kind.is_empty() && kind != "tree" {
                return None;
            }
            let committish =
                if kind.is_empty() { fragment } else { Some(at(4)).filter(|c| !c.is_empty()).map(String::from) };
            (at(1).to_string(), strip_git(at(2)).to_string(), committish)
        }
        Host::Bitbucket | Host::SourceHut => {
            let blocked = if host == Host::Bitbucket { "get" } else { "archive" };
            if at(3) == blocked {
                return None;
            }
            (at(1).to_string(), strip_git(at(2)).to_string(), fragment)
        }
        Host::GitLab => {
            let path = pathname.strip_prefix('/').unwrap_or(pathname);
            if path.contains("/-/") || path.contains("/archive.tar.gz") {
                return None;
            }
            let (user, project) = path.rsplit_once('/').unwrap_or(("", path));
            (user.to_string(), strip_git(project).to_string(), fragment)
        }
    };
    (!user.is_empty() && !project.is_empty()).then_some((user, project, committish))
}

fn is_github_shorthand(value: &str) -> bool {
    let slashes = value.matches('/').count();
    slashes == 1
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.starts_with('.')
        && !value.contains(['@', ':'])
        && !value.chars().any(char::is_whitespace)
}

/// `hosted-git-info` `fromUrl` for the supported hosts.
fn hosted(candidate: &str) -> Option<Hosted> {
    let (base, fragment) = candidate.split_once('#').unwrap_or((candidate, ""));
    let shortcuts = [("github:", Host::GitHub), ("gitlab:", Host::GitLab), ("bitbucket:", Host::Bitbucket)];
    let shortcut = shortcuts.iter().find_map(|(prefix, host)| Some((*host, base.strip_prefix(prefix)?)));
    let (host, pathname, shortcut) = if let Some((host, rest)) = shortcut {
        (host, format!("/{rest}"), true)
    } else if is_github_shorthand(base) {
        (Host::GitHub, format!("/{base}"), true)
    } else if let Some((user_host, path)) = base.split_once(':').filter(|_| !base.contains("://")) {
        let (_, domain) = user_host.split_once('@')?;
        (Host::from_domain(domain)?, format!("/{path}"), false)
    } else {
        let url = Url::parse(base).ok()?;
        let protocols = ["git", "http", "https", "ssh", "git+ssh", "git+https"];
        if !protocols.contains(&url.scheme()) {
            return None;
        }
        (Host::from_domain(url.host_str()?)?, url.path().to_string(), false)
    };
    let (user, project, committish) = extract(host, &pathname, fragment)?;
    Some(Hosted { host, user, project, committish, shortcut })
}

fn parse_generic_git_url(url: &str) -> Option<GitSource> {
    let (repo_without_ref, git_ref) = split_ref(url);
    let mut repo = repo_without_ref.clone();
    let (host, path) = if let Some((host, path)) = scp_like(&repo_without_ref) {
        (host.to_string(), path.to_string())
    } else if ["https://", "http://", "ssh://", "git://"].iter().any(|prefix| repo_without_ref.starts_with(prefix)) {
        let parsed = Url::parse(&repo_without_ref).ok()?;
        (parsed.host_str().unwrap_or_default().to_string(), parsed.path().trim_start_matches('/').to_string())
    } else {
        let (host, path) = repo_without_ref.split_once('/')?;
        if !host.contains('.') && host != "localhost" {
            return None;
        }
        repo = format!("https://{repo_without_ref}");
        (host.to_string(), path.to_string())
    };
    build_git_source(repo, &host, &path, git_ref)
}

fn has_protocol(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    ["http://", "https://", "ssh://", "git://"].iter().any(|prefix| lower.starts_with(prefix))
}

/// Parse a git source. With a `git:` prefix every historical shorthand form is accepted;
/// without it only `http`, `https`, `ssh`, and `git` protocol URLs are.
pub fn parse_git_url(source: &str) -> Option<GitSource> {
    let trimmed = source.trim();
    let url = trimmed.strip_prefix("git:").map(str::trim);
    let has_prefix = url.is_some();
    let url = url.unwrap_or(trimmed);
    if !has_prefix && !has_protocol(url) {
        return None;
    }
    let (split_repo, split_ref) = split_ref(url);
    let with_ref = |base: &str| split_ref.as_ref().map(|git_ref| format!("{base}#{git_ref}"));
    let https_repo = format!("https://{split_repo}");
    let candidates = [
        (with_ref(&split_repo), false),
        (Some(url.to_string()), false),
        (with_ref(&https_repo), true),
        (Some(format!("https://{url}")), true),
    ];
    for (candidate, https_candidate) in candidates {
        let Some(info) = candidate.as_deref().and_then(hosted) else { continue };
        if split_ref.is_some() && info.project.contains('@') {
            continue;
        }
        let protocol_prefixes = ["http://", "https://", "ssh://", "git://", "git@"];
        let repo = if info.shortcut {
            // Upstream prefixes the shortcut itself; a usable clone URL names the host.
            format!("https://{}/{}/{}", info.host.domain(), info.user, info.project)
        } else if https_candidate || !protocol_prefixes.iter().any(|prefix| split_repo.starts_with(prefix)) {
            https_repo.clone()
        } else {
            split_repo.clone()
        };
        let path = format!("{}/{}", info.user, info.project);
        return build_git_source(repo, info.host.domain(), &path, info.committish.or_else(|| split_ref.clone()));
    }
    parse_generic_git_url(url)
}
