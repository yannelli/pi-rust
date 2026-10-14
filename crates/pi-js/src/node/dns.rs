//! Natives for `node:dns`: `lookup` and `lookupService` through the system resolver
//! (getaddrinfo and getnameinfo), and DNS queries for the resolve family on hickory-resolver.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use hickory_resolver::config::{NameServerConfigGroup, ResolverConfig, ResolverOpts};
use hickory_resolver::name_server::TokioConnectionProvider;
use hickory_resolver::proto::op::ResponseCode;
use hickory_resolver::proto::rr::{RData, Record, RecordType};
use hickory_resolver::proto::{ProtoError, ProtoErrorKind};
use hickory_resolver::{Name, ResolveError, ResolveErrorKind, TokioResolver};
use rquickjs::function::Opt;
use rquickjs::{Ctx, Object, Value};
use serde_json::{Value as Json, json};

use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{from_json, throw};
use crate::runtime::scope::spawn_op;
use crate::runtime::state::state;

struct Channel {
    resolver: Arc<TokioResolver>,
    servers: Vec<SocketAddr>,
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

fn system_config() -> (ResolverConfig, ResolverOpts) {
    hickory_resolver::system_conf::read_system_conf()
        .unwrap_or_else(|_| (ResolverConfig::from_parts(None, Vec::new(), Vec::new()), ResolverOpts::default()))
}

fn build(servers: Option<Vec<SocketAddr>>, timeout: Option<u64>, tries: Option<usize>) -> Channel {
    let (system, mut options) = system_config();
    let servers = servers.unwrap_or_else(|| {
        let mut list: Vec<SocketAddr> = system.name_servers().iter().map(|server| server.socket_addr).collect();
        list.dedup();
        list
    });
    let mut group = NameServerConfigGroup::new();
    for server in &servers {
        group.merge(NameServerConfigGroup::from_ips_clear(&[server.ip()], server.port(), true));
    }
    let config = ResolverConfig::from_parts(None, Vec::new(), group);
    options.cache_size = 0;
    options.use_hosts_file = hickory_resolver::config::ResolveHosts::Never;
    if let Some(timeout) = timeout {
        options.timeout = Duration::from_millis(timeout);
    }
    if let Some(tries) = tries {
        options.attempts = tries.max(1);
    }
    let resolver =
        TokioResolver::builder_with_config(config, TokioConnectionProvider::default()).with_options(options).build();
    Channel { resolver: Arc::new(resolver), servers }
}

fn query_error(error: &ResolveError, syscall: &str, hostname: &str) -> JsError {
    let proto: Option<&ProtoError> = match error.kind() {
        ResolveErrorKind::Proto(proto) => Some(proto),
        _ => None,
    };
    let code = match proto.map(ProtoError::kind) {
        Some(ProtoErrorKind::NoRecordsFound { response_code, .. }) => match *response_code {
            ResponseCode::NXDomain => "ENOTFOUND",
            ResponseCode::NoError => "ENODATA",
            ResponseCode::Refused => "EREFUSED",
            ResponseCode::NotImp => "ENOTIMP",
            ResponseCode::FormErr => "EFORMERR",
            _ => "ESERVFAIL",
        },
        Some(ProtoErrorKind::Timeout) => "ETIMEOUT",
        Some(ProtoErrorKind::NoConnections | ProtoErrorKind::Io(_)) => "ECONNREFUSED",
        _ => "EBADRESP",
    };
    dns_error(code, syscall, hostname)
}

fn name_text(name: &Name) -> String {
    name.to_utf8().trim_end_matches('.').to_string()
}

fn bytes_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// One record in Node's shape for `resolve<type>`; `any` adds the `type` key.
fn record_json(record: &Record, ttl: bool, any: bool) -> Option<Json> {
    let mut value = match record.data() {
        RData::A(address) if ttl || any => json!({ "address": address.0.to_string(), "ttl": record.ttl() }),
        RData::AAAA(address) if ttl || any => json!({ "address": address.0.to_string(), "ttl": record.ttl() }),
        RData::A(address) => json!(address.0.to_string()),
        RData::AAAA(address) => json!(address.0.to_string()),
        RData::MX(mx) => json!({ "exchange": name_text(mx.exchange()), "priority": mx.preference() }),
        RData::TXT(txt) => {
            let chunks: Vec<String> = txt.txt_data().iter().map(|chunk| bytes_text(chunk)).collect();
            if any { json!({ "entries": chunks }) } else { json!(chunks) }
        }
        RData::SRV(srv) => json!({
            "name": name_text(srv.target()), "port": srv.port(), "priority": srv.priority(), "weight": srv.weight(),
        }),
        RData::NS(ns) if any => json!({ "value": name_text(&ns.0) }),
        RData::CNAME(cname) if any => json!({ "value": name_text(&cname.0) }),
        RData::PTR(ptr) if any => json!({ "value": name_text(&ptr.0) }),
        RData::NS(ns) => json!(name_text(&ns.0)),
        RData::CNAME(cname) => json!(name_text(&cname.0)),
        RData::PTR(ptr) => json!(name_text(&ptr.0)),
        RData::SOA(soa) => json!({
            "nsname": name_text(soa.mname()), "hostmaster": name_text(soa.rname()), "serial": soa.serial(),
            "refresh": soa.refresh(), "retry": soa.retry(), "expire": soa.expire(), "minttl": soa.minimum(),
        }),
        RData::CAA(caa) => {
            let mut object = serde_json::Map::new();
            object.insert("critical".into(), json!(if caa.issuer_critical() { 128 } else { 0 }));
            object.insert(caa.tag().as_str().to_string(), json!(bytes_text(caa.raw_value())));
            Json::Object(object)
        }
        RData::NAPTR(naptr) => json!({
            "flags": bytes_text(naptr.flags()), "service": bytes_text(naptr.services()),
            "regexp": bytes_text(naptr.regexp()), "replacement": name_text(naptr.replacement()),
            "order": naptr.order(), "preference": naptr.preference(),
        }),
        _ => return None,
    };
    if any {
        value["type"] = json!(record.record_type().to_string());
    }
    Some(value)
}

fn channel(ctx: &Ctx<'_>, id: u64) -> rquickjs::Result<Arc<TokioResolver>> {
    state().with_resource(id, |channel: &mut Channel| channel.resolver.clone()).map_err(|error| throw(ctx, &error))
}

fn servers_json(servers: &[SocketAddr]) -> Json {
    json!(servers.iter().map(|server| [server.ip().to_string(), server.port().to_string()]).collect::<Vec<_>>())
}

fn parse_servers(ctx: &Ctx<'_>, list: Vec<(String, u16)>) -> rquickjs::Result<Vec<SocketAddr>> {
    let parse = |(address, port): (String, u16)| {
        IpAddr::from_str(&address).map(|ip| SocketAddr::new(ip, port)).map_err(|_| {
            throw(
                ctx,
                &JsError::type_error(format!("Invalid IP address: {address}")).with_code("ERR_INVALID_IP_ADDRESS"),
            )
        })
    };
    list.into_iter().map(parse).collect()
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
    set_fn(&object, "createChannel", |ctx: Ctx<'js>, servers: Value<'js>, timeout: Opt<f64>, tries: Opt<f64>| {
        let servers: Option<Vec<(String, u16)>> = super::net::parse_options(&ctx, servers)?;
        let servers = servers.map(|list| parse_servers(&ctx, list)).transpose()?;
        let timeout = timeout.0.filter(|value| *value >= 0.0).map(|value| value as u64);
        let channel = build(servers, timeout, tries.0.filter(|value| *value > 0.0).map(|value| value as usize));
        let servers = servers_json(&channel.servers);
        let id = state().insert_resource(Box::new(channel));
        from_json(&ctx, &json!({ "id": id, "servers": servers }))
    })?;
    set_fn(&object, "closeChannel", |id: u64| {
        state().resources.borrow_mut().remove(&id);
    })?;
    set_fn(&object, "query", |ctx: Ctx<'js>, id: u64, kind: String, name: String, ttl: bool| {
        let resolver = channel(&ctx, id)?;
        let syscall = format!("query{}", if kind == "ANY" { "Any".to_string() } else { kind.clone() });
        let record_type = RecordType::from_str(&kind)
            .map_err(|_| throw(&ctx, &JsError::type_error(format!("Unknown type {kind}"))))?;
        let work = async move {
            let fqdn = if name.ends_with('.') { name.clone() } else { format!("{name}.") };
            let found =
                resolver.lookup(fqdn, record_type).await.map_err(|error| query_error(&error, &syscall, &name))?;
            let any = record_type == RecordType::ANY;
            let records: Vec<Json> = found
                .record_iter()
                .filter(|record| any || record.record_type() == record_type)
                .filter_map(|record| record_json(record, ttl, any))
                .collect();
            if records.is_empty() {
                return Err(dns_error("ENODATA", &syscall, &name));
            }
            Ok(if record_type == RecordType::SOA { records[0].clone() } else { json!(records) })
        };
        spawn_op(&ctx, work, |ctx, value| {
            from_json(ctx, &value).map_err(|error| crate::runtime::convert::caught(ctx, error))
        })
    })?;
    set_fn(&object, "reverse", |ctx: Ctx<'js>, id: u64, address: String| -> rquickjs::Result<Value<'js>> {
        let resolver = channel(&ctx, id)?;
        let ip = IpAddr::from_str(&address)
            .map_err(|_| throw(&ctx, &JsError::new(format!("getHostByAddr EINVAL {address}")).with_code("EINVAL")))?;
        let work = async move {
            let found =
                resolver.reverse_lookup(ip).await.map_err(|error| query_error(&error, "getHostByAddr", &address))?;
            Ok(found.iter().map(|ptr| name_text(&ptr.0)).collect::<Vec<_>>())
        };
        spawn_op(&ctx, work, |ctx, list| {
            from_json(ctx, &json!(list)).map_err(|error| crate::runtime::convert::caught(ctx, error))
        })
        .map(|promise| promise.into_value())
    })?;
    Ok(object)
}
