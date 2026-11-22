//! System address lookups and DNS record queries for `node:dns`.

mod config;
mod resolver;
mod wire;

use std::borrow::Cow;
use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rquickjs::{Ctx, Object, Value};
use serde_json::{Value as Json, json};
use tokio_util::sync::CancellationToken;

use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{caught, from_json, throw};
use crate::runtime::scope::spawn_op;
use resolver::Resolver;
use wire::Kind;

struct Channel {
    resolver: Arc<Resolver>,
    local: (Ipv4Addr, Ipv6Addr),
    cancel: CancellationToken,
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn dns_error(code: &str, syscall: &str, hostname: &str) -> JsError {
    JsError::new(format!("{syscall} {code} {hostname}"))
        .with_code(code)
        .with_data("syscall", syscall)
        .with_data("hostname", hostname)
}

fn lookup(host: String) -> Result<Vec<(String, u8)>, JsError> {
    let found = (host.as_str(), 0).to_socket_addrs().map_err(|error| {
        let text = error.to_string().to_lowercase();
        let (code, errno) = if text.contains("temporary") || text.contains("try again") {
            ("EAI_AGAIN", -3001)
        } else {
            ("ENOTFOUND", -3008)
        };
        dns_error(code, "getaddrinfo", &host).with_data("errno", errno)
    })?;
    let mut list: Vec<(String, u8)> = Vec::new();
    for address in found {
        let entry = (address.ip().to_string(), if address.is_ipv4() { 4 } else { 6 });
        if !list.contains(&entry) {
            list.push(entry);
        }
    }
    Ok(list)
}

fn servers_json(servers: &[SocketAddr]) -> Vec<String> {
    servers
        .iter()
        .map(|server| {
            if server.port() == 53 {
                server.ip().to_string()
            } else {
                SocketAddr::new(server.ip(), server.port()).to_string()
            }
        })
        .collect()
}

fn parse_servers(ctx: &Ctx<'_>, list: Vec<(String, u16)>) -> rquickjs::Result<Vec<SocketAddr>> {
    let mut servers = Vec::with_capacity(list.len());
    for (address, port) in list {
        let address = config::address(&address, port).ok_or_else(|| {
            throw(
                ctx,
                &JsError::type_error(format!("Invalid IP address: {address}")).with_code("ERR_INVALID_IP_ADDRESS"),
            )
        })?;
        if !servers.contains(&address) {
            servers.push(address);
        }
    }
    Ok(servers)
}

fn ip_address(text: &str) -> Option<IpAddr> {
    text.parse().ok().or_else(|| text.split_once('%')?.0.parse::<Ipv6Addr>().ok().map(IpAddr::V6))
}

fn reverse_name(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, d] = ip.octets();
            format!("{d}.{c}.{b}.{a}.in-addr.arpa")
        }
        IpAddr::V6(ip) => {
            let mut name = String::with_capacity(72);
            for digit in format!("{:032x}", ip.to_bits()).chars().rev() {
                name.push(digit);
                name.push('.');
            }
            name.push_str("ip6.arpa");
            name
        }
    }
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "lookup", |ctx: Ctx<'js>, host: String| {
        let work = async move {
            tokio::task::spawn_blocking(move || lookup(host)).await.map_err(|error| JsError::new(error.to_string()))?
        };
        spawn_op(&ctx, work, |ctx, list| {
            from_json(ctx, &json!(list)).map_err(|error| crate::runtime::convert::caught(ctx, error))
        })
    })?;
    set_fn(&object, "lookupService", |ctx: Ctx<'js>, address: String, port: u16| {
        let ip = IpAddr::from_str(&address).map_err(|_| throw(&ctx, &JsError::type_error("Invalid IP address")))?;
        let work = async move {
            let _ = ("localhost", 0).to_socket_addrs();
            let found = tokio::task::spawn_blocking(move || super::sys::name_info(SocketAddr::new(ip, port))).await;
            let found = found.map_err(|error| JsError::new(error.to_string()))?;
            found.map_err(|code| dns_error(code, "getnameinfo", &address))
        };
        spawn_op(&ctx, work, |ctx, pair| {
            from_json(ctx, &json!(pair)).map_err(|error| crate::runtime::convert::caught(ctx, error))
        })
    })?;
    set_fn(&object, "createChannel", create_channel)?;
    Ok(object)
}

fn create_channel<'js>(
    ctx: Ctx<'js>,
    values: Value<'js>,
    timeout: i32,
    tries: u32,
    max_timeout: u32,
) -> rquickjs::Result<Object<'js>> {
    let list: Option<Vec<(String, u16)>> = super::net::parse_options(&ctx, values)?;
    let config =
        if list.is_none() || timeout <= 0 { config::system(list.is_none()) } else { config::Config::default() };
    let servers = match list {
        Some(list) => parse_servers(&ctx, list)?,
        None => config.servers,
    };
    let displayed = servers_json(&servers);
    let timeout = if timeout > 0 { Duration::from_millis(timeout as u64) } else { config.timeout };
    let max_timeout = (max_timeout > 0).then(|| Duration::from_millis(u64::from(max_timeout)));
    let resolver = Resolver::new(servers, timeout, tries as usize, max_timeout);
    let channel = Channel {
        resolver: Arc::new(resolver),
        local: (Ipv4Addr::UNSPECIFIED, Ipv6Addr::UNSPECIFIED),
        cancel: CancellationToken::new(),
    };
    let channel = Rc::new(RefCell::new(channel));
    let object = Object::new(ctx.clone())?;
    object.set("servers", displayed)?;
    let servers_channel = Rc::clone(&channel);
    set_fn(&object, "setServers", move |ctx: Ctx<'js>, values: Value<'js>| {
        let servers = parse_servers(&ctx, super::net::parse_options(&ctx, values)?)?;
        let displayed = servers_json(&servers);
        let mut channel = servers_channel.borrow_mut();
        channel.resolver = Arc::new(channel.resolver.with_servers(servers));
        Ok::<_, rquickjs::Error>(displayed)
    })?;
    let local_channel = Rc::clone(&channel);
    set_fn(&object, "setLocalAddress", move |ctx: Ctx<'js>, first: String, other: Option<String>| {
        let parse = |text: &str| {
            ip_address(text).ok_or_else(|| {
                throw(&ctx, &JsError::type_error("Invalid IP address.").with_code("ERR_INVALID_ARG_VALUE"))
            })
        };
        let first = parse(&first)?;
        // Node applies the first address even when the second address is invalid.
        match first {
            IpAddr::V4(ip) => local_channel.borrow_mut().local.0 = ip,
            IpAddr::V6(ip) => local_channel.borrow_mut().local.1 = ip,
        }
        let local = match (first, other.as_deref().map(parse).transpose()?) {
            (IpAddr::V4(v4), Some(IpAddr::V6(v6))) | (IpAddr::V6(v6), Some(IpAddr::V4(v4))) => (v4, v6),
            (IpAddr::V4(v4), None) => (v4, Ipv6Addr::UNSPECIFIED),
            (IpAddr::V6(v6), None) => (Ipv4Addr::UNSPECIFIED, v6),
            _ => {
                let family = if first.is_ipv4() { "IPv4" } else { "IPv6" };
                return Err(throw(
                    &ctx,
                    &JsError::type_error(format!("Cannot specify two {family} addresses."))
                        .with_code("ERR_INVALID_ARG_VALUE"),
                ));
            }
        };
        local_channel.borrow_mut().local = local;
        Ok(())
    })?;
    let cancel_channel = Rc::clone(&channel);
    set_fn(&object, "cancel", move || std::mem::take(&mut cancel_channel.borrow_mut().cancel).cancel())?;
    set_fn(&object, "query", move |ctx: Ctx<'js>, kind: String, host: String, ttl: bool| {
        let (resolver, local, cancel) = {
            let channel = channel.borrow();
            (Arc::clone(&channel.resolver), channel.local, channel.cancel.clone())
        };
        let (kind, query, syscall) = if kind == "REVERSE" {
            let ip = ip_address(&host).ok_or_else(|| throw(&ctx, &dns_error("EINVAL", "getHostByAddr", &host)))?;
            (Kind::Reverse, Some(reverse_name(ip)), "getHostByAddr".to_string())
        } else {
            let record_type =
                Kind::parse(&kind).ok_or_else(|| throw(&ctx, &JsError::type_error("Unknown record type")))?;
            let syscall = format!("query{}{}", &kind[..1], kind[1..].to_ascii_lowercase());
            (record_type, None, syscall)
        };
        let work = async move {
            let query = match query {
                Some(ptr) => Cow::Owned(ptr),
                None => idna::domain_to_ascii_cow(host.as_bytes(), idna::AsciiDenyList::EMPTY).unwrap_or_default(),
            };
            resolver.resolve(&query, kind, ttl, local, cancel).await.map_err(|code| {
                let code = if kind == Kind::Reverse && !matches!(code, "ECANCELLED" | "ENODATA" | "ENOMEM") {
                    "ENOTFOUND"
                } else {
                    code
                };
                dns_error(code, &syscall, &host)
            })
        };
        spawn_op(&ctx, work, |ctx, value: Json| from_json(ctx, &value).map_err(|error| caught(ctx, error)))
    })?;
    Ok(object)
}
