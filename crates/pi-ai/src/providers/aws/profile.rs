//! Shared AWS config files: `~/.aws/config` and `~/.aws/credentials`, or the paths in
//! `AWS_CONFIG_FILE` and `AWS_SHARED_CREDENTIALS_FILE`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use crate::providers::provider_env::{home_dir, provider_env_value};

pub type Section = HashMap<String, String>;

/// Parsed sections of both files. Profile sections merge with credentials file values winning.
#[derive(Clone, Debug, Default)]
pub struct SharedConfig {
    pub profiles: HashMap<String, Section>,
    pub sso_sessions: HashMap<String, Section>,
}

impl SharedConfig {
    pub fn load(env: &BTreeMap<String, String>) -> Self {
        let home = home_dir(env).unwrap_or_default();
        let path = |name: &str, default: &str| -> PathBuf {
            match provider_env_value(name, env) {
                Some(path) => expand_home(&path, &home),
                None => home.join(".aws").join(default),
            }
        };
        let read = |path: PathBuf| std::fs::read_to_string(path).unwrap_or_default();
        Self::parse(&read(path("AWS_CONFIG_FILE", "config")), &read(path("AWS_SHARED_CREDENTIALS_FILE", "credentials")))
    }

    pub fn parse(config: &str, credentials: &str) -> Self {
        let mut shared = Self::default();
        for (name, section) in parse_ini(config) {
            if name == "default" {
                merge(&mut shared.profiles, "default", section);
            } else if let Some(profile) = name.strip_prefix("profile ") {
                merge(&mut shared.profiles, profile.trim(), section);
            } else if let Some(session) = name.strip_prefix("sso-session ") {
                merge(&mut shared.sso_sessions, session.trim(), section);
            }
        }
        for (name, section) in parse_ini(credentials) {
            let name = name.strip_prefix("profile ").unwrap_or(&name).trim().to_string();
            merge(&mut shared.profiles, &name, section);
        }
        shared
    }

    pub fn profile(&self, name: &str) -> Option<&Section> {
        self.profiles.get(name)
    }
}

fn merge(target: &mut HashMap<String, Section>, name: &str, section: Section) {
    target.entry(name.to_string()).or_default().extend(section);
}

fn expand_home(path: &str, home: &std::path::Path) -> PathBuf {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => home.join(rest),
        None => PathBuf::from(path),
    }
}

/// INI sections in file order. Indented lines after `key =` are nested settings and skipped.
fn parse_ini(text: &str) -> Vec<(String, Section)> {
    let mut sections: Vec<(String, Section)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            sections.push((name.trim().to_string(), Section::new()));
            continue;
        }
        if raw.starts_with([' ', '\t']) {
            continue;
        }
        let (Some((_, section)), Some((key, value))) = (sections.last_mut(), line.split_once('=')) else { continue };
        let value = strip_inline_comment(value.trim());
        section.insert(key.trim().to_ascii_lowercase(), value.to_string());
    }
    sections
}

fn strip_inline_comment(value: &str) -> &str {
    let cut = [" #", "\t#", " ;", "\t;"].iter().filter_map(|marker| value.find(marker)).min();
    cut.map_or(value, |index| value[..index].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_config_and_credentials_profiles() {
        let config = "[default]\nregion = us-west-2\n\n[profile dev]\nregion=eu-west-1 # comment\nrole_arn = arn:aws:iam::1:role/r\nsource_profile = base\ns3 =\n  max_concurrent_requests = 20\n[sso-session corp]\nsso_region = us-east-1\nsso_start_url = https://corp.awsapps.com/start\n";
        let credentials =
            "[base]\naws_access_key_id = AKIA\naws_secret_access_key = secret\n; comment\n[dev]\nregion = ap-south-1\n";
        let shared = SharedConfig::parse(config, credentials);
        assert_eq!(shared.profile("default").unwrap()["region"], "us-west-2");
        let dev = shared.profile("dev").unwrap();
        assert_eq!(dev["region"], "ap-south-1");
        assert_eq!(dev["source_profile"], "base");
        assert!(!dev.contains_key("max_concurrent_requests"));
        assert_eq!(shared.profile("base").unwrap()["aws_secret_access_key"], "secret");
        assert_eq!(shared.sso_sessions["corp"]["sso_region"], "us-east-1");
    }
}
