// Adapted from c-ares 1.34.9 ares_hosts_file.c. See LICENSE-c-ares in the dns directory.

//! Hosts file entries for `reverse`, parsed and merged as c-ares 1.34 `ares_hosts_file.c` does:
//! lines that share an address or a hostname become one entry.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// c-ares returns at most 100 aliases from one entry.
const MAX_ALIASES: usize = 100;
/// `INET6_ADDRSTRLEN` and the 256-byte hostname buffer, each with room for the NUL.
const MAX_ADDRESS: usize = 45;
const MAX_NAME: usize = 255;

#[derive(Default)]
pub(super) struct Hosts {
    entries: Vec<Entry>,
    by_ip: HashMap<IpAddr, usize>,
    by_name: HashMap<String, usize>,
}

#[derive(Default)]
struct Entry {
    ips: Vec<IpAddr>,
    names: Vec<String>,
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\x0b' | b'\x0c' | b'\r')
}

fn is_name(text: &str) -> bool {
    text.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'/' | b'*'))
}

fn printable(token: &[u8], max: usize) -> Option<&str> {
    let printable = token.len() <= max && token.iter().all(|byte| (0x20..=0x7e).contains(byte));
    printable.then(|| std::str::from_utf8(token).ok()).flatten()
}

/// Whitespace-separated tokens up to a token that starts with `#`.
fn tokens(line: &[u8]) -> impl Iterator<Item = &[u8]> {
    line.split(|byte| is_space(*byte)).filter(|token| !token.is_empty()).take_while(|token| token[0] != b'#')
}

fn parse_line(line: &[u8]) -> Option<Entry> {
    let mut tokens = tokens(line);
    let ip = super::hosts_address::parse(printable(tokens.next()?, MAX_ADDRESS)?)?;
    let mut names = Vec::new();
    for token in tokens {
        let Some(name) = printable(token, MAX_NAME) else {
            if names.is_empty() {
                return None;
            }
            continue;
        };
        // c-ares compares a new name with the entry's addresses, so repeated names stay.
        if is_name(name) && !name.eq_ignore_ascii_case(&ip.to_string()) {
            names.push(name.to_string());
        }
    }
    (!names.is_empty()).then(|| Entry { ips: vec![ip], names })
}

impl Hosts {
    pub(super) fn parse(text: &[u8]) -> Self {
        let mut hosts = Self::default();
        for entry in text.split(|byte| *byte == b'\n').filter_map(parse_line) {
            hosts.add(entry);
        }
        hosts
    }

    fn add(&mut self, mut entry: Entry) {
        let added = entry.names.len();
        let by_ip = entry.ips.iter().find_map(|ip| self.by_ip.get(ip).copied());
        let by_name = || entry.names.iter().find_map(|name| self.by_name.get(&name.to_ascii_lowercase()).copied());
        let index = match by_ip.or_else(by_name) {
            Some(index) => {
                if by_ip.is_none() {
                    let ips = entry.ips.iter().filter(|ip| !self.by_ip.contains_key(ip));
                    let ips: Vec<IpAddr> = ips.copied().collect();
                    self.entries[index].ips.extend(ips);
                }
                let names = entry.names.drain(..).filter(|name| !self.by_name.contains_key(&name.to_ascii_lowercase()));
                let names: Vec<String> = names.collect();
                self.entries[index].names.extend(names);
                index
            }
            None => {
                self.entries.push(entry);
                self.entries.len() - 1
            }
        };
        if by_ip.is_none()
            && let Some(&ip) = self.entries[index].ips.last()
        {
            self.by_ip.entry(ip).or_insert(index);
        }
        let names = &self.entries[index].names;
        for name in names.iter().rev().take(added) {
            self.by_name.entry(name.to_ascii_lowercase()).or_insert(index);
        }
    }

    /// The aliases c-ares reports for `ip`: every name after the first.
    pub(super) fn aliases(&self, ip: IpAddr) -> Option<Vec<String>> {
        let entry = &self.entries[*self.by_ip.get(&ip)?];
        Some(entry.names.iter().skip(1).take(MAX_ALIASES).cloned().collect())
    }
}

struct Cached {
    loaded: u64,
    hosts: Arc<Hosts>,
}

/// The parsed hosts file of one channel, as c-ares keeps `channel->hf`.
#[derive(Default)]
pub(super) struct Cache {
    path: OnceLock<Option<PathBuf>>,
    cached: Mutex<Option<Cached>>,
}

fn seconds(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

impl Cache {
    /// Aliases for `ip`; `None` when the file has no entry for it or cannot be read.
    pub(super) fn aliases(&self, ip: IpAddr) -> Option<Vec<String>> {
        let path = self.path.get_or_init(super::hosts_path::path).as_deref()?;
        self.load(path)?.aliases(ip)
    }

    /// Reads `path` again when its modification second is at or after the second of the last
    /// load, or every 60 seconds when it cannot be checked, as `ares_hosts_expired` does.
    fn load(&self, path: &Path) -> Option<Arc<Hosts>> {
        let now = seconds(SystemTime::now());
        let modified = std::fs::metadata(path).and_then(|meta| meta.modified()).map_or(now.saturating_sub(60), seconds);
        let mut cache = self.cached.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cached) = cache.as_ref()
            && cached.loaded > modified
        {
            return Some(Arc::clone(&cached.hosts));
        }
        *cache = None;
        let hosts = Arc::new(Hosts::parse(&std::fs::read(path).ok()?));
        *cache = Some(Cached { loaded: now, hosts: Arc::clone(&hosts) });
        Some(hosts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases(text: &str, ip: &str) -> Option<Vec<String>> {
        Hosts::parse(text.as_bytes()).aliases(ip.parse().unwrap())
    }

    #[test]
    fn one_name_gives_no_aliases_and_lines_merge_by_address_and_name() {
        let text = "127.0.0.1 localhost\n255.255.255.255 broadcasthost\n::1 localhost\n";
        assert_eq!(aliases(text, "127.0.0.1"), Some(vec![]));
        assert_eq!(aliases(text, "::1"), Some(vec![]));
        let text = "192.168.1.1  host.example.com host\n192.168.1.5  host.example.com host\n\
            2620:1234::1 host.example.com host6.example.com host6 host\n192.168.1.1 extra\n";
        let expected = ["host", "host6.example.com", "host6", "extra"].map(String::from).to_vec();
        assert_eq!(aliases(text, "192.168.1.5"), Some(expected.clone()));
        assert_eq!(aliases(text, "2620:1234:0::1"), Some(expected));
        assert_eq!(aliases(text, "192.168.1.9"), None);
    }

    #[test]
    fn skips_comments_bad_lines_and_invalid_names() {
        let text = "127.1 short\n# 10.0.0.1 a b\n  10.0.0.2\tfirst a#b second #third\r\n10.0.0.3 bad!name\n\
            300.0.0.1 x y\n10.0.0.4 bad!name good alias DUP dup\n10.0.0.5 \x01 ok\n10.0.0.6 a \x01 b\n";
        assert_eq!(aliases(text, "10.0.0.1"), None);
        assert_eq!(aliases(text, "127.1.0.0"), Some(vec![]));
        assert_eq!(aliases(text, "10.0.0.2"), Some(vec!["second".to_string()]));
        assert_eq!(aliases(text, "10.0.0.3"), None);
        assert_eq!(aliases(text, "10.0.0.4"), Some(["alias", "DUP", "dup"].map(String::from).to_vec()));
        assert_eq!(aliases(text, "10.0.0.5"), None);
        assert_eq!(aliases(text, "10.0.0.6"), Some(vec!["b".to_string()]));
    }

    #[test]
    fn names_claimed_by_another_entry_are_dropped_and_aliases_are_capped() {
        let text = "10.0.0.1 one shared\n10.0.0.2 two\n10.0.0.2 shared three\n";
        assert_eq!(aliases(text, "10.0.0.2"), Some(vec!["three".to_string()]));
        assert_eq!(aliases(text, "10.0.0.1"), Some(vec!["shared".to_string()]));
        let names: Vec<String> = (0..150).map(|n| format!("n{n}")).collect();
        let found = aliases(&format!("10.0.0.9 {}\n", names.join(" ")), "10.0.0.9").unwrap();
        assert_eq!((found.len(), found[0].as_str(), found[99].as_str()), (100, "n1", "n100"));
    }

    #[test]
    fn reloads_after_the_file_changes() {
        let dir = std::env::temp_dir().join(format!("pi-dns-hosts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hosts");
        std::fs::write(&file, "10.1.0.1 a b\n").unwrap();
        let ip = "10.1.0.1".parse().unwrap();
        let cache = Cache::default();
        assert_eq!(cache.load(&file).unwrap().aliases(ip), Some(vec!["b".to_string()]));
        std::fs::write(&file, "10.1.0.1 a c\n").unwrap();
        assert_eq!(cache.load(&file).unwrap().aliases(ip), Some(vec!["c".to_string()]));
        assert!(Cache::default().load(&dir.join("missing")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
