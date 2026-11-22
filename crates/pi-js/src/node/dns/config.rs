use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

pub(super) struct Config {
    pub servers: Vec<SocketAddr>,
    pub timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self { servers: Vec::new(), timeout: Duration::from_secs(2) }
    }
}

pub(super) fn system(include_servers: bool) -> Config {
    let mut config = platform(include_servers);
    if include_servers && config.servers.is_empty() {
        config.servers.push(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 53));
    }
    config
}

pub(super) fn address(text: &str, port: u16) -> Option<SocketAddr> {
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, port));
    }
    let (ip, zone) = text.split_once('%')?;
    let scope = zone.parse::<u32>().ok().or_else(|| {
        let index = super::super::dgram_ssm::interface_index(zone);
        (index != 0).then_some(index)
    })?;
    Some(SocketAddr::V6(SocketAddrV6::new(ip.parse().ok()?, port, 0, scope)))
}

#[cfg(not(windows))]
fn resolv_conf() -> Config {
    let contents = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    parse_resolv_conf(&contents)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform(_: bool) -> Config {
    resolv_conf()
}

#[cfg(target_os = "macos")]
fn platform(include_servers: bool) -> Config {
    let mut config = resolv_conf();
    if include_servers
        && let Ok(output) = std::process::Command::new("/usr/sbin/scutil").arg("--dns").env("LC_ALL", "C").output()
        && output.status.success()
        && let Ok(text) = std::str::from_utf8(&output.stdout)
        && text.starts_with("DNS configuration")
    {
        config.servers = macos_servers(text);
    }
    config
}

#[cfg(any(target_os = "macos", test))]
fn macos_servers(text: &str) -> Vec<SocketAddr> {
    let mut servers = Vec::new();
    let global = text.split("\nDNS configuration (").next().unwrap_or(text);
    for resolver in global.split("\nresolver #").skip(1) {
        let mut names = Vec::new();
        let mut domain = false;
        let mut reachable = false;
        let mut port = 53;
        let mut interface = 0;
        for (key, value) in resolver.lines().filter_map(|line| line.split_once(':')) {
            let value = value.split_whitespace().next().unwrap_or_default();
            match key.trim() {
                "domain" => domain = true,
                "reach" => {
                    // SystemConfiguration's Reachable and ConnectionOnTraffic flags.
                    reachable = value
                        .strip_prefix("0x")
                        .and_then(|s| u32::from_str_radix(s, 16).ok())
                        .is_some_and(|flags| flags & 0x0a != 0);
                }
                "port" => port = value.parse().ok().filter(|n| *n != 0).unwrap_or(53),
                "if_index" => interface = value.parse().unwrap_or(0),
                key if key.starts_with("nameserver[") => names.push(value),
                _ => {}
            }
        }
        if domain || !reachable {
            continue;
        }
        for mut server in names.into_iter().filter_map(|text| address(text, port)) {
            if let SocketAddr::V6(ip) = &mut server
                && ip.ip().is_unicast_link_local()
                && ip.scope_id() == 0
            {
                ip.set_scope_id(interface);
            }
            if !servers.contains(&server) {
                servers.push(server);
            }
        }
    }
    servers
}

#[cfg(any(not(windows), test))]
fn parse_resolv_conf(contents: &str) -> Config {
    let mut config = Config::default();
    for line in contents.lines() {
        let mut words = line.split(['#', ';']).next().unwrap_or_default().split_whitespace();
        match words.next() {
            Some("nameserver") => {
                if let Some(server) = words.next().and_then(|s| address(s, 53))
                    && !config.servers.contains(&server)
                {
                    config.servers.push(server);
                }
            }
            Some("options") => {
                for option in words {
                    if let Some(seconds) = option.strip_prefix("timeout:").or_else(|| option.strip_prefix("retrans:"))
                        && let Ok(value) = seconds.parse::<u32>()
                        && value > 0
                    {
                        config.timeout = Duration::from_secs(u64::from(value));
                    }
                }
            }
            _ => {}
        }
    }
    config
}

#[cfg(windows)]
fn platform(include_servers: bool) -> Config {
    use std::net::Ipv6Addr;
    if !include_servers {
        return Config::default();
    }
    let mut ranked = Vec::new();
    for adapter in ipconfig::get_adapters().unwrap_or_default() {
        if adapter.oper_status() != ipconfig::OperStatus::IfOperStatusUp {
            continue;
        }
        for &ip in adapter.dns_servers() {
            let entry = match ip {
                IpAddr::V4(v4) if v4.is_unspecified() || v4.is_broadcast() => continue,
                IpAddr::V4(_) => (adapter.ipv4_metric(), SocketAddr::new(ip, 53)),
                IpAddr::V6(v6) => {
                    if matches!(v6.segments(), [0xfec0, 0, 0, 0xffff, 0, 0, 0, 1..=3]) || v6 == Ipv6Addr::UNSPECIFIED {
                        continue;
                    }
                    let scope = if v6.is_unicast_link_local() { adapter.ipv6_if_index() } else { 0 };
                    (adapter.ipv6_metric(), SocketAddr::V6(SocketAddrV6::new(v6, 53, 0, scope)))
                }
            };
            ranked.push(entry);
        }
    }
    ranked.sort_by_key(|entry| entry.0);
    let mut config = Config::default();
    for (_, server) in ranked {
        if !config.servers.contains(&server) {
            config.servers.push(server);
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_uses_reachable_global_resolvers_including_vpn_servers() {
        let text = "DNS configuration\nresolver #1\nsearch domain[0] : corp.test\n nameserver[0] : 192.0.2.2\n reach : 0x00000002\n\
            resolver #2\n nameserver[0] : fe80::3\n if_index : 7 (en1)\n port : 5353\n reach : 0x00000008\n\
            resolver #3\n domain : local\n nameserver[0] : 192.0.2.4\n reach : 0x00000002\n\
            resolver #4\n nameserver[0] : 192.0.2.5\n reach : 0x00000000\n\
            DNS configuration (for scoped queries)\nresolver #1\n nameserver[0] : 192.0.2.6\n reach : 0x00000002\n";
        assert_eq!(macos_servers(text), ["192.0.2.2:53".parse().unwrap(), "[fe80::3%7]:5353".parse().unwrap()]);
    }

    #[test]
    fn reads_ordered_servers_and_timeout_ignoring_comments_and_search_domains() {
        let config = parse_resolv_conf(
            "search ignored.test\n nameserver 192.0.2.1 # primary\nnameserver fe80::2%7;backup\n\
             nameserver nonsense\nnameserver 192.0.2.1\noptions ndots:2 timeout:3 timeout:bad\n",
        );
        assert_eq!(config.servers, ["192.0.2.1:53".parse().unwrap(), "[fe80::2%7]:53".parse().unwrap()]);
        assert_eq!(config.timeout, Duration::from_secs(3));
    }
}
