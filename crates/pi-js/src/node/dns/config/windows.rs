// Adapted from c-ares 1.34.9 ares_sysconfig_win.c. See LICENSE-c-ares in the dns directory.

//! Windows DNS servers in c-ares 1.34 `get_DNS_Windows` order: best route metric to each server
//! on its adapter plus the interface metric. Routes come from `netsh` only when two servers differ.
//! `netsh` starts from the kernel's `\SystemRoot` link, so `SystemRoot` and `PATH` do not pick it.

use std::net::{IpAddr, SocketAddr};

/// One DNS server of an adapter that is up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Candidate {
    pub server: SocketAddr,
    /// Interface index that `netsh` reports in its `Idx` column, 0 when unknown.
    pub interface: u32,
    pub interface_metric: u32,
}

#[derive(Debug, PartialEq)]
pub(super) struct Route {
    prefix: IpAddr,
    length: u8,
    metric: u32,
    interface: u32,
}

type Tables = (Vec<Route>, Vec<(u32, String)>);

/// Routes from `netsh interface ipv4|ipv6 show route`, with the interface name of each
/// on-link row. Columns are found by shape, so localized headers do not matter.
pub(super) fn parse_routes(text: &str) -> Tables {
    let mut routes = Vec::new();
    let mut names = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(at) = words.iter().position(|word| word.contains('/')) else { continue };
        let (Some(metric), Some(interface)) = (
            at.checked_sub(1).and_then(|before| words[before].parse().ok()),
            words.get(at + 1).and_then(|word| word.parse().ok()),
        ) else {
            continue;
        };
        let Some((prefix, length)) = words[at].split_once('/') else { continue };
        let (Ok(prefix), Ok(length)) = (prefix.parse::<IpAddr>(), length.parse::<u8>()) else { continue };
        if length > if prefix.is_ipv4() { 32 } else { 128 } {
            continue;
        }
        routes.push(Route { prefix, length, metric, interface });
        let name = words[at + 2..].join(" ");
        if !name.is_empty() && name.parse::<IpAddr>().is_err() && !names.iter().any(|(index, _)| *index == interface) {
            names.push((interface, name));
        }
    }
    (routes, names)
}

fn contains(route: &Route, address: IpAddr) -> bool {
    let bits = |ip: IpAddr| match ip {
        IpAddr::V4(ip) => u128::from(ip.to_bits()) << 96,
        IpAddr::V6(ip) => ip.to_bits(),
    };
    let mask = u128::MAX.checked_shl(128 - u32::from(route.length)).unwrap_or(0);
    route.prefix.is_ipv4() == address.is_ipv4() && (bits(route.prefix) ^ bits(address)) & mask == 0
}

/// `getBestRouteMetric`: the longest matching route on the interface, lowest metric first,
/// plus the interface metric. `u32::MAX` when no route matches or the sum overflows.
pub(super) fn best_metric(routes: &[Route], candidate: &Candidate) -> u32 {
    let destination = candidate.server.ip();
    let best = routes
        .iter()
        .filter(|route| route.interface == candidate.interface && contains(route, destination))
        .min_by_key(|route| (std::cmp::Reverse(route.length), route.metric));
    match best {
        Some(route) if route.metric != u32::MAX => route.metric.saturating_add(candidate.interface_metric),
        _ => u32::MAX,
    }
}

/// Stable sort by metric, then drop repeats of an address, port, and scope.
pub(super) fn rank(mut ranked: Vec<(u32, SocketAddr)>) -> Vec<SocketAddr> {
    ranked.sort_by_key(|entry| entry.0);
    let mut servers = Vec::with_capacity(ranked.len());
    for (_, server) in ranked {
        if !servers.contains(&server) {
            servers.push(server);
        }
    }
    servers
}

/// Ranks `candidates` with route tables; without them each server gets its interface metric.
pub(super) fn order(candidates: &[Candidate], tables: Option<&Tables>) -> Vec<SocketAddr> {
    let routes = tables.map(|(routes, _)| routes.as_slice()).unwrap_or_default();
    let has_v4 = routes.iter().any(|route| route.prefix.is_ipv4());
    let has_v6 = routes.iter().any(|route| route.prefix.is_ipv6());
    let ranked = candidates.iter().map(|candidate| {
        let has_table = if candidate.server.is_ipv4() { has_v4 } else { has_v6 };
        let metric = if has_table && candidate.interface != 0 {
            best_metric(routes, candidate)
        } else {
            candidate.interface_metric
        };
        (metric, candidate.server)
    });
    rank(ranked.collect())
}

#[cfg(windows)]
pub(super) fn servers() -> Vec<SocketAddr> {
    use std::net::SocketAddrV6;
    let mut candidates = Vec::new();
    let mut adapters = Vec::new();
    for adapter in ipconfig::get_adapters().unwrap_or_default() {
        if adapter.oper_status() != ipconfig::OperStatus::IfOperStatusUp {
            continue;
        }
        for &ip in adapter.dns_servers() {
            let (server, interface_metric) = match ip {
                IpAddr::V4(v4) if v4.is_unspecified() || v4.is_broadcast() => continue,
                IpAddr::V4(_) => (SocketAddr::new(ip, 53), adapter.ipv4_metric()),
                IpAddr::V6(v6) if v6.is_unspecified() => continue,
                IpAddr::V6(v6) => {
                    let scope = if v6.is_unicast_link_local() { adapter.ipv6_if_index() } else { 0 };
                    (SocketAddr::V6(SocketAddrV6::new(v6, 53, 0, scope)), adapter.ipv6_metric())
                }
            };
            candidates.push(Candidate { server, interface: adapter.ipv6_if_index(), interface_metric });
            adapters.push(adapter.friendly_name().to_string());
        }
    }
    let first = candidates.first().map(|candidate| candidate.server);
    if candidates.iter().all(|candidate| Some(candidate.server) == first) {
        return first.into_iter().collect();
    }
    let tables = route_tables(&candidates);
    if let Some((_, names)) = &tables {
        // ipconfig has no IPv4 interface index; the on-link rows name it.
        for (candidate, adapter) in candidates.iter_mut().zip(&adapters) {
            if let Some((index, _)) = names.iter().find(|(_, name)| name == adapter)
                && candidate.server.is_ipv4()
            {
                candidate.interface = *index;
            }
        }
    }
    order(&candidates, tables.as_ref())
}

/// Runs `netsh` for each address family in use, both at once and each within the helper
/// deadline. `None` when no table could be read.
#[cfg(windows)]
fn route_tables(candidates: &[Candidate]) -> Option<Tables> {
    use super::bounded;
    let show = |family: &str| {
        let mut command = super::system_root::command("netsh.exe");
        command.args(["interface", family, "show", "route"]);
        bounded::run(command, bounded::HELPER_DEADLINE, bounded::MAX_HELPER_OUTPUT)
            .filter(|output| output.success)
            .map(|output| parse_routes(&String::from_utf8_lossy(&output.stdout)))
    };
    let families = [("ipv4", true), ("ipv6", false)]
        .into_iter()
        .filter(|(_, v4)| candidates.iter().any(|candidate| candidate.server.is_ipv4() == *v4));
    let read: Vec<Option<Tables>> = std::thread::scope(|scope| {
        let workers: Vec<_> = families.map(|(family, _)| scope.spawn(move || show(family))).collect();
        workers.into_iter().map(|worker| worker.join().ok().flatten()).collect()
    });
    let mut tables: Tables = (Vec::new(), Vec::new());
    for (routes, names) in read.into_iter().flatten() {
        tables.0.extend(routes);
        tables.1.extend(names);
    }
    (!tables.0.is_empty()).then_some(tables)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IPV4: &str = "\nPublish  Type      Met  Prefix                    Idx  Gateway/Interface Name\n\
        -------  --------  ---  ------------------------  ---  ------------------------\n\
        No       Manual    0    0.0.0.0/0                   7  192.168.1.1\n\
        No       System    256  192.168.1.0/24              7  Ethernet 2\n\
        Nein     System    256  10.8.0.0/16                12  VPN\n\
        No       Manual    0    0.0.0.0/0                  12  10.8.0.1\n\
        No       System    256  127.0.0.0/8                 1  Loopback Pseudo-Interface 1\n";
    const IPV6: &str = "Publish  Type      Met  Prefix                    Idx  Gateway/Interface Name\n\
        No       System    256  fe80::/64                   7  Ethernet 2\n\
        No       Manual    16   ::/0                        7  fe80::1\n";

    fn candidate(server: &str, interface: u32, interface_metric: u32) -> Candidate {
        Candidate { server: server.parse().unwrap(), interface, interface_metric }
    }

    fn addresses(list: &[&str]) -> Vec<SocketAddr> {
        list.iter().map(|text| text.parse().unwrap()).collect()
    }

    #[test]
    fn parses_netsh_rows_and_on_link_interface_names() {
        let (routes, names) = parse_routes(IPV4);
        assert_eq!(routes.len(), 5);
        assert_eq!(routes[1], Route { prefix: "192.168.1.0".parse().unwrap(), length: 24, metric: 256, interface: 7 });
        assert_eq!(names, [(7, "Ethernet 2".into()), (12, "VPN".into()), (1, "Loopback Pseudo-Interface 1".into())]);
        assert_eq!(parse_routes(IPV6).0.len(), 2);
    }

    #[test]
    fn longest_prefix_wins_then_the_interface_metric_is_added() {
        let (routes, _) = parse_routes(IPV4);
        assert_eq!(best_metric(&routes, &candidate("192.168.1.53:53", 7, 25)), 281);
        assert_eq!(best_metric(&routes, &candidate("8.8.8.8:53", 7, 25)), 25);
        assert_eq!(best_metric(&routes, &candidate("8.8.8.8:53", 9, 25)), u32::MAX);
        assert_eq!(best_metric(&routes, &candidate("8.8.8.8:53", 7, u32::MAX)), u32::MAX);
        let (routes, _) = parse_routes(IPV6);
        assert_eq!(best_metric(&routes, &candidate("[fe80::53%7]:53", 7, 10)), 266);
        assert_eq!(best_metric(&routes, &candidate("[2001:db8::53]:53", 7, 10)), 26);
    }

    #[test]
    fn the_missing_route_family_falls_back_to_its_interface_metric() {
        let servers = [candidate("192.168.1.53:53", 7, 25), candidate("[fe80::53%7]:53", 7, 10)];
        assert_eq!(order(&servers, Some(&parse_routes(IPV4))), [servers[1].server, servers[0].server]);
        assert_eq!(order(&servers, Some(&parse_routes(IPV6))), [servers[0].server, servers[1].server]);
    }

    #[test]
    fn orders_by_route_metric_keeping_scopes_and_the_first_duplicate() {
        let mut tables = parse_routes(IPV4);
        tables.0.extend(parse_routes(IPV6).0);
        let candidates = [
            candidate("192.168.1.53:53", 7, 25),
            candidate("[fe80::53%7]:53", 7, 10),
            candidate("10.8.0.53:53", 12, 5),
            candidate("9.9.9.9:53", 7, 25),
            candidate("10.8.0.53:53", 12, 5),
            candidate("203.0.113.1:53", 30, 1),
        ];
        let ranked = ["9.9.9.9:53", "10.8.0.53:53", "[fe80::53%7]:53", "192.168.1.53:53", "203.0.113.1:53"];
        assert_eq!(order(&candidates, Some(&tables)), addresses(&ranked));
        let by_interface = ["203.0.113.1:53", "10.8.0.53:53", "[fe80::53%7]:53", "192.168.1.53:53", "9.9.9.9:53"];
        assert_eq!(order(&candidates, None), addresses(&by_interface));
    }
}
