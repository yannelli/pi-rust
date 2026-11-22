//! System resolver settings as c-ares 1.34 `ares_init_by_sysconfig` reads them: platform
//! servers and options, then `RES_OPTIONS`, which overrides them.

#[cfg(any(windows, test))]
mod windows;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

/// One source in the c-ares `lookups` string: `f` for the hosts file, `b` for DNS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Lookup {
    Files,
    Dns,
}

#[derive(Debug, PartialEq)]
pub(super) struct Config {
    pub servers: Vec<SocketAddr>,
    pub timeout: Duration,
    pub rotate: bool,
    pub lookups: Vec<Lookup>,
}

impl Default for Config {
    fn default() -> Self {
        Self { servers: Vec::new(), timeout: Duration::from_secs(2), rotate: false, lookups: DEFAULT_LOOKUPS.to_vec() }
    }
}

const DEFAULT_LOOKUPS: [Lookup; 2] = [Lookup::Files, Lookup::Dns];

pub(super) fn system(include_servers: bool) -> Config {
    let mut config = platform(include_servers);
    if let Some(options) = std::env::var_os("RES_OPTIONS") {
        set_options(&mut config, &options.to_string_lossy());
    }
    config.servers.retain(|server| !blocked(server.ip()));
    if include_servers && config.servers.is_empty() {
        config.servers.push(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 53));
    }
    config
}

/// `ares_server_blacklisted`: deprecated site-local `fec0::/10` servers are skipped.
fn blocked(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfec0)
}

#[cfg(any(not(windows), test))]
fn address(text: &str, port: u16) -> Option<SocketAddr> {
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, port));
    }
    let (ip, zone) = text.split_once('%')?;
    let scope = zone.parse::<u32>().ok().or_else(|| {
        let index = super::super::dgram_ssm::interface_index(zone);
        (index != 0).then_some(index)
    })?;
    Some(SocketAddr::V6(std::net::SocketAddrV6::new(ip.parse().ok()?, port, 0, scope)))
}

/// `ares_sysconfig_set_options`: `timeout`/`retrans` and `rotate`; bad values are skipped.
fn set_options(config: &mut Config, text: &str) {
    for option in text.split([' ', '\t']).filter(|option| !option.is_empty()) {
        let (key, value) = option.split_once(':').map_or((option, ""), |(key, value)| (key.trim(), value.trim()));
        let value = match value {
            "" => None,
            digits if digits.bytes().all(|byte| byte.is_ascii_digit()) => match digits.parse::<u32>() {
                Ok(value) => Some(value),
                Err(_) => continue,
            },
            _ => continue,
        };
        match (key, value) {
            ("timeout" | "retrans", Some(seconds)) if seconds > 0 => {
                config.timeout = Duration::from_secs(u64::from(seconds));
            }
            ("rotate", _) => config.rotate = true,
            _ => {}
        }
    }
}

/// `config_lookup`: known words become sources, repeats are dropped, and a list with no known
/// word leaves the earlier order in place.
#[cfg(any(not(windows), test))]
fn set_lookups(config: &mut Config, value: &str, separators: &[char]) {
    let mut lookups = Vec::new();
    for word in value.split(separators).map(str::trim) {
        let lookup = match word.to_ascii_lowercase().as_str() {
            "dns" | "bind" | "bind4" | "bind6" | "resolv" | "resolve" => Lookup::Dns,
            "files" | "file" | "local" | "local4" | "local6" => Lookup::Files,
            _ => continue,
        };
        if !lookups.contains(&lookup) {
            lookups.push(lookup);
        }
    }
    if !lookups.is_empty() {
        config.lookups = lookups;
    }
}

#[cfg(any(not(any(windows, target_os = "macos")), test))]
fn parse_lookup_files(config: &mut Config, nsswitch: &str, svc: &[String]) {
    for line in nsswitch.lines().map(str::trim).filter(|line| !line.starts_with('#')) {
        if let Some((key, value)) = line.split_once(':')
            && key.trim() == "hosts"
        {
            set_lookups(config, value, &[' ', '\t']);
        }
    }
    for line in svc.iter().flat_map(|text| text.lines()).map(str::trim).filter(|line| !line.starts_with('#')) {
        if let Some((key, value)) = line.split_once('=')
            && key.trim() == "hosts"
        {
            set_lookups(config, value, &[',']);
        }
    }
}

#[cfg(not(windows))]
fn resolv_conf() -> Config {
    let contents = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    parse_resolv_conf(&contents)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform(_: bool) -> Config {
    let mut config = resolv_conf();
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    parse_lookup_files(&mut config, &read("/etc/nsswitch.conf"), &[read("/etc/netsvc.conf"), read("/etc/svc.conf")]);
    config
}

/// c-ares reads only `dnsinfo` on macOS, which `scutil --dns` prints; every channel takes its
/// options there. The generated `/etc/resolv.conf` is used only when `scutil` fails.
#[cfg(target_os = "macos")]
fn platform(include_servers: bool) -> Config {
    let output = std::process::Command::new("/usr/sbin/scutil").arg("--dns").env("LC_ALL", "C").output();
    let mut config = match output {
        Ok(output) if output.status.success() && output.stdout.starts_with(b"DNS configuration") => {
            macos_config(&String::from_utf8_lossy(&output.stdout))
        }
        _ => resolv_conf(),
    };
    config.lookups = DEFAULT_LOOKUPS.to_vec();
    if !include_servers {
        config.servers.clear();
    }
    config
}

#[cfg(any(target_os = "macos", test))]
fn macos_config(text: &str) -> Config {
    let mut config = Config::default();
    let global = text.split("\nDNS configuration (").next().unwrap_or(text);
    for resolver in global.split("\nresolver #").skip(1) {
        let mut names = Vec::new();
        let mut options = Vec::new();
        let mut domain = false;
        let mut reachable = false;
        let mut port = 53;
        let mut interface = 0;
        for (key, value) in resolver.lines().filter_map(|line| line.split_once(':')) {
            let first = value.split_whitespace().next().unwrap_or_default();
            match key.trim() {
                "domain" => domain = true,
                "reach" => {
                    // SystemConfiguration's Reachable and ConnectionOnTraffic flags.
                    reachable = first
                        .strip_prefix("0x")
                        .and_then(|s| u32::from_str_radix(s, 16).ok())
                        .is_some_and(|flags| flags & 0x0a != 0);
                }
                "port" => port = first.parse().ok().filter(|n| *n != 0).unwrap_or(53),
                "if_index" => interface = first.parse().unwrap_or(0),
                "options" => options.push(value.trim()),
                key if key.starts_with("nameserver[") => names.push(first),
                _ => {}
            }
        }
        if domain || !reachable || names.is_empty() {
            continue;
        }
        for text in options {
            set_options(&mut config, text);
        }
        for mut server in names.into_iter().filter_map(|text| address(text, port)) {
            if let SocketAddr::V6(ip) = &mut server
                && ip.ip().is_unicast_link_local()
                && ip.scope_id() == 0
            {
                ip.set_scope_id(interface);
            }
            if !config.servers.contains(&server) {
                config.servers.push(server);
            }
        }
    }
    config
}

/// `ares_sysconfig_parse_resolv_line` for `nameserver`, `options`, and `lookup`.
#[cfg(any(not(windows), test))]
fn parse_resolv_conf(contents: &str) -> Config {
    let mut config = Config::default();
    for line in contents.lines().map(str::trim) {
        if line.starts_with(['#', ';']) {
            continue;
        }
        let (keyword, rest) = line.split_once([' ', '\t', '\x0b', '\x0c', '\r']).unwrap_or((line, ""));
        let value = rest.split(['#', ';']).next().unwrap_or_default().trim();
        if value.is_empty() {
            continue;
        }
        match keyword {
            "nameserver" => {
                if let Some(server) = value.split_whitespace().next().and_then(|s| address(s, 53))
                    && !config.servers.contains(&server)
                {
                    config.servers.push(server);
                }
            }
            "options" => set_options(&mut config, value),
            "lookup" | "hostresorder" => set_lookups(&mut config, value, &[' ', '\t']),
            _ => {}
        }
    }
    config
}

#[cfg(windows)]
fn platform(include_servers: bool) -> Config {
    if !include_servers {
        return Config::default();
    }
    Config { servers: windows::servers(), ..Config::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_uses_reachable_global_resolvers_including_vpn_servers() {
        let text = "DNS configuration\nresolver #1\nsearch domain[0] : corp.test\n nameserver[0] : 192.0.2.2\n reach : 0x00000002\n\
            options : timeout:4 rotate\n\
            resolver #2\n nameserver[0] : fe80::3\n if_index : 7 (en1)\n port : 5353\n reach : 0x00000008\n\
            resolver #3\n domain : local\n nameserver[0] : 192.0.2.4\n reach : 0x00000002\n options : timeout:9\n\
            resolver #4\n nameserver[0] : 192.0.2.5\n reach : 0x00000000\n\
            DNS configuration (for scoped queries)\nresolver #1\n nameserver[0] : 192.0.2.6\n reach : 0x00000002\n";
        let config = macos_config(text);
        assert_eq!(config.servers, ["192.0.2.2:53".parse().unwrap(), "[fe80::3%7]:5353".parse().unwrap()]);
        assert_eq!(
            (config.timeout, config.rotate, config.lookups),
            (Duration::from_secs(4), true, DEFAULT_LOOKUPS.to_vec())
        );
    }

    #[test]
    fn reads_ordered_servers_options_and_lookup_ignoring_comments_and_search_domains() {
        let config = parse_resolv_conf(
            "search ignored.test\n nameserver 192.0.2.1 # primary\nnameserver fe80::2%7;backup\n\
             nameserver nonsense\nnameserver 192.0.2.1\noptions ndots:2 timeout:3 timeout:bad timeout:0\n\
             ;options rotate\nlookup bind file\n",
        );
        assert_eq!(config.servers, ["192.0.2.1:53".parse().unwrap(), "[fe80::2%7]:53".parse().unwrap()]);
        assert_eq!((config.timeout, config.rotate), (Duration::from_secs(3), false));
        assert_eq!(config.lookups, [Lookup::Dns, Lookup::Files]);
        let rotated = parse_resolv_conf("options rotate:2 attempts:3\noptions\trotate\nlookup mdns\n");
        assert_eq!((rotated.rotate, rotated.lookups), (true, DEFAULT_LOOKUPS.to_vec()));
    }

    #[test]
    fn later_lookup_files_replace_the_order() {
        let mut config = parse_resolv_conf("lookup file\n");
        parse_lookup_files(&mut config, "# hosts: files\nhosts:  dns [NOTFOUND=return] files mdns4\n", &[]);
        assert_eq!(config.lookups, [Lookup::Dns, Lookup::Files]);
        parse_lookup_files(&mut config, "hosts: mdns4_minimal\n", &["hosts = local , bind\n".into()]);
        assert_eq!(config.lookups, [Lookup::Files, Lookup::Dns]);
        parse_lookup_files(&mut config, "", &["hosts = bind6\n".into(), "#hosts = local\n".into()]);
        assert_eq!(config.lookups, [Lookup::Dns]);
    }

    #[test]
    fn res_options_overrides_and_site_local_servers_are_dropped() {
        let mut config = Config::default();
        set_options(&mut config, "rotate\ttimeout:7 retrans:x");
        assert_eq!((config.timeout, config.rotate), (Duration::from_secs(7), true));
        assert!(blocked("fec0:0:0:ffff::1".parse().unwrap()) && blocked("feff::1".parse().unwrap()));
        assert!(!blocked("fe80::1".parse().unwrap()) && !blocked("192.0.2.1".parse().unwrap()));
    }
}
