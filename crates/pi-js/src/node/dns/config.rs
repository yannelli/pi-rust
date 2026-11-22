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

pub(super) fn system() -> Config {
    let mut config = platform();
    if config.servers.is_empty() {
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
fn platform() -> Config {
    let contents = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    parse_resolv_conf(&contents)
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
fn platform() -> Config {
    use std::net::Ipv6Addr;
    let mut ranked = Vec::new();
    for adapter in ipconfig::get_adapters().unwrap_or_default() {
        if adapter.oper_status() != ipconfig::OperStatus::IfOperStatusUp {
            continue;
        }
        for &ip in adapter.dns_servers() {
            let entry = match ip {
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
    fn reads_ordered_servers_and_timeout_ignoring_comments_and_search_domains() {
        let config = parse_resolv_conf(
            "search ignored.test\n nameserver 192.0.2.1 # primary\nnameserver fe80::2%7;backup\n\
             nameserver nonsense\nnameserver 192.0.2.1\noptions ndots:2 timeout:3 timeout:bad\n",
        );
        assert_eq!(config.servers, ["192.0.2.1:53".parse().unwrap(), "[fe80::2%7]:53".parse().unwrap()]);
        assert_eq!(config.timeout, Duration::from_secs(3));
    }
}
