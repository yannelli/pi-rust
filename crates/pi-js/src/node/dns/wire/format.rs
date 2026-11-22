//! Node 22 result shapes for each record type, CNAME chains inside one response, and the
//! `resolveAny` order.

use std::net::Ipv6Addr;

use serde_json::{Map, Value as Json, json};

use super::name::Name;
use super::record::{Data, Record};
use super::{BADRESP, CLASS_IN, Kind, NODATA, Reply};

/// CNAME hops followed inside one response; the caller bounds hops across responses.
const MAX_CHAIN: usize = 16;

/// `resolveAny` lists A (or the CNAME), then these kinds, and only the first SOA.
const ANY_ORDER: [Kind; 10] =
    [Kind::Aaaa, Kind::Mx, Kind::Ns, Kind::Txt, Kind::Srv, Kind::Ptr, Kind::Naptr, Kind::Soa, Kind::Tlsa, Kind::Caa];

/// The question name and the CNAME targets reached from it, in order. `ttl` is the lowest
/// CNAME TTL, starting from the cap carried over from earlier responses.
struct Chain {
    names: Vec<Name>,
    ttl: u32,
}

impl Chain {
    fn follow(question: &Name, answers: &[Record], cap: u32) -> Self {
        let mut names = vec![question.clone()];
        let mut ttl = cap;
        while names.len() <= MAX_CHAIN {
            let current = &names[names.len() - 1];
            let hop = answers.iter().find_map(|record| match &record.data {
                Data::Cname(target) if record.class == CLASS_IN && record.owner.same(current) => {
                    Some((target, record.ttl))
                }
                _ => None,
            });
            let Some((target, hop_ttl)) = hop else { break };
            if names.iter().any(|name| name.same(target)) {
                break;
            }
            ttl = ttl.min(hop_ttl);
            names.push(target.clone());
        }
        Self { names, ttl }
    }

    fn owns(&self, record: &Record) -> bool {
        record.class == CLASS_IN && self.names.iter().any(|name| name.same(&record.owner))
    }

    fn first_target(&self) -> Option<&Name> {
        self.names.get(1)
    }
}

fn latin1(bytes: &[u8]) -> Json {
    Json::String(bytes.iter().copied().map(char::from).collect())
}

/// libuv writes an IPv4-compatible address (`::a.b.c.d`) in dotted form; Rust does not.
fn ipv6_text(address: &Ipv6Addr) -> String {
    let words = address.segments();
    if words[..6] == [0; 6] && words[6] != 0 {
        let [.., a, b, c, d] = address.octets();
        return format!("::{a}.{b}.{c}.{d}");
    }
    address.to_string()
}

/// The bare value `resolve<Type>` gives for A and AAAA without TTLs, NS, PTR, CNAME and TXT.
fn bare(data: &Data) -> Option<Json> {
    Some(match data {
        Data::A(address) => Json::String(address.to_string()),
        Data::Aaaa(address) => Json::String(ipv6_text(address)),
        Data::Cname(name) | Data::Ns(name) | Data::Ptr(name) => Json::String(name.text()),
        Data::Txt(chunks) => chunks.iter().map(|chunk| latin1(chunk)).collect(),
        _ => return None,
    })
}

/// One record as an object, as `resolveAny` lists it before the `type` key.
fn object(record: &Record, chain: &Chain) -> Map<String, Json> {
    let value = match &record.data {
        Data::A(_) | Data::Aaaa(_) => json!({ "address": bare(&record.data), "ttl": record.ttl.min(chain.ttl) }),
        Data::Cname(_) | Data::Ns(_) | Data::Ptr(_) => json!({ "value": bare(&record.data) }),
        Data::Txt(_) => json!({ "entries": bare(&record.data) }),
        Data::Mx { priority, exchange } => json!({ "exchange": exchange.text(), "priority": priority }),
        Data::Srv { priority, weight, port, target } => {
            json!({ "name": target.text(), "port": port, "priority": priority, "weight": weight })
        }
        Data::Naptr { order, preference, flags, service, regexp, replacement } => json!({
            "flags": latin1(flags), "service": latin1(service), "regexp": latin1(regexp),
            "replacement": replacement.text(), "order": order, "preference": preference,
        }),
        Data::Soa { nsname, hostmaster, serial, refresh, retry, expire, minttl } => json!({
            "nsname": nsname.text(), "hostmaster": hostmaster.text(), "serial": serial,
            "refresh": refresh, "retry": retry, "expire": expire, "minttl": minttl,
        }),
        Data::Caa { critical, tag, value } => {
            let mut object = Map::new();
            object.insert("critical".into(), json!(critical));
            object.insert(tag.iter().copied().map(char::from).collect(), latin1(value));
            return object;
        }
        Data::Tlsa { usage, selector, matching, data } => {
            json!({ "certUsage": usage, "selector": selector, "match": matching, "data": data })
        }
        Data::Other => return Map::new(),
    };
    match value {
        Json::Object(object) => object,
        _ => Map::new(),
    }
}

fn matching<'a>(chain: &'a Chain, answers: &'a [Record], kind: Kind) -> impl Iterator<Item = &'a Record> + 'a {
    answers.iter().filter(move |record| record.data.kind() == Some(kind) && chain.owns(record))
}

/// The shape `resolve<Type>` returns for one record.
fn single(record: &Record, chain: &Chain, ttl: bool) -> Json {
    match &record.data {
        Data::A(_) | Data::Aaaa(_) if ttl => Json::Object(object(record, chain)),
        data => bare(data).unwrap_or_else(|| Json::Object(object(record, chain))),
    }
}

/// Node 22 stores each TLSA object at the list length before the first one plus its
/// answer-section index; the gaps are holes in Node and `null` here.
fn tlsa_slots(chain: &Chain, answers: &[Record], list: &mut Vec<Json>) {
    let offset = list.len();
    let found = answers.iter().enumerate().filter(|(_, record)| record.data.kind() == Some(Kind::Tlsa));
    for (index, record) in found.filter(|(_, record)| chain.owns(record)) {
        list.resize(offset + index, Json::Null);
        list.push(Json::Object(object(record, chain)));
    }
}

fn any(chain: &Chain, answers: &[Record]) -> Json {
    let tagged = |kind: Kind, record: &Record| {
        let mut object = object(record, chain);
        object.insert("type".into(), json!(kind.label()));
        Json::Object(object)
    };
    let mut list = Vec::new();
    match chain.first_target() {
        Some(target) => list.push(json!({ "value": target.text(), "type": "CNAME" })),
        None => list.extend(matching(chain, answers, Kind::A).map(|record| tagged(Kind::A, record))),
    }
    for kind in ANY_ORDER {
        let mut found = matching(chain, answers, kind).map(|record| tagged(kind, record));
        match kind {
            Kind::Soa => list.extend(found.next()),
            // Node 22 adds no `type` key to TLSA entries.
            Kind::Tlsa => tlsa_slots(chain, answers, &mut list),
            _ => list.extend(found),
        }
    }
    Json::Array(list)
}

/// Formats a NOERROR answer section that has at least one record. `cap` limits A and AAAA
/// TTLs with the CNAME TTLs of earlier responses.
pub(super) fn reply(
    kind: Kind,
    question: &Name,
    answers: &[Record],
    ttl: bool,
    cap: u32,
) -> Result<Reply, &'static str> {
    let kind = if kind == Kind::Reverse { Kind::Ptr } else { kind };
    let chain = Chain::follow(question, answers, cap);
    match kind {
        Kind::Any => return Ok(Reply::Answer(any(&chain, answers))),
        Kind::Cname => {
            let target = chain
                .first_target()
                .or_else(|| answers.iter().any(|r| chain.owns(r)).then_some(question))
                .ok_or(NODATA)?;
            return Ok(Reply::Answer(json!([target.text()])));
        }
        Kind::Tlsa => {
            let mut list = Vec::new();
            tlsa_slots(&chain, answers, &mut list);
            if !list.is_empty() {
                return Ok(Reply::Answer(Json::Array(list)));
            }
        }
        _ => {
            let mut found = matching(&chain, answers, kind).map(|record| single(record, &chain, ttl)).peekable();
            if found.peek().is_some() {
                let value = if kind == Kind::Soa { found.next().unwrap_or(Json::Null) } else { found.collect() };
                return Ok(Reply::Answer(value));
            }
        }
    }
    if chain.names.len() > 1 {
        return Ok(Reply::Alias { name: chain.names[chain.names.len() - 1].text(), ttl: chain.ttl });
    }
    // Node 22 answers these differently when the section holds only other types.
    match kind {
        Kind::Soa => Err(BADRESP),
        Kind::A | Kind::Aaaa | Kind::Ns | Kind::Ptr => Err(NODATA),
        _ => Ok(Reply::Answer(json!([]))),
    }
}
