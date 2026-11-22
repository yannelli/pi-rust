//! Node 22 result shapes for each record type, as the c-ares 1.34 legacy parsers and Node's
//! `cares_wrap.cc` build them from one answer section. Owner names are not compared.

use std::net::Ipv6Addr;

use serde_json::{Map, Value as Json, json};

use super::name::Name;
use super::record::{Data, Record};
use super::{BADRESP, CLASS_IN, Kind, NODATA};

const CLASS_CHAOS: u16 = 3;

/// `resolveAny` lists A (or the CNAME), then these kinds.
const ANY_ORDER: [Kind; 10] =
    [Kind::Aaaa, Kind::Mx, Kind::Ns, Kind::Txt, Kind::Srv, Kind::Ptr, Kind::Naptr, Kind::Soa, Kind::Tlsa, Kind::Caa];

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

/// Sections with an incomplete escape have already failed validation.
fn copied(name: &Name) -> String {
    name.copied_text().unwrap_or_default()
}

/// One record as an object, as `resolveAny` lists it before the `type` key. `cap` is the
/// lowest CNAME TTL in the answer; it limits A and AAAA TTLs.
fn object(record: &Record, cap: u32) -> Map<String, Json> {
    let value = match &record.data {
        Data::A(_) | Data::Aaaa(_) => json!({ "address": bare(&record.data), "ttl": record.ttl.min(cap) }),
        Data::Cname(_) | Data::Ns(_) | Data::Ptr(_) => json!({ "value": bare(&record.data) }),
        Data::Txt(_) => json!({ "entries": bare(&record.data) }),
        Data::Mx { priority, exchange } => json!({ "exchange": exchange.text(), "priority": priority }),
        Data::Srv { priority, weight, port, target } => {
            json!({ "name": copied(target), "port": port, "priority": priority, "weight": weight })
        }
        Data::Naptr { order, preference, flags, service, regexp, replacement } => json!({
            "flags": latin1(flags), "service": latin1(service), "regexp": latin1(regexp),
            "replacement": copied(replacement), "order": order, "preference": preference,
        }),
        // Node passes refresh, retry and expire through a signed 32-bit integer.
        Data::Soa { nsname, hostmaster, serial, refresh, retry, expire, minttl } => json!({
            "nsname": nsname.text(), "hostmaster": hostmaster.text(), "serial": serial,
            "refresh": *refresh as i32, "retry": *retry as i32, "expire": *expire as i32, "minttl": minttl,
        }),
        Data::Caa { critical, tag, value } => {
            // Node reads the value as a C string, which ends at the first NUL byte.
            let value = value.split(|byte| *byte == 0).next().unwrap_or_default();
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

/// The classes each c-ares parser reads: IN, plus CHAOS for TXT and CAA. Node reads TLSA
/// records of every class, and `resolveAny` reads the first SOA of every class.
fn class_matches(kind: Kind, class: u16, any: bool) -> bool {
    match kind {
        Kind::Tlsa => true,
        Kind::Soa if any => true,
        Kind::Txt | Kind::Caa => matches!(class, CLASS_IN | CLASS_CHAOS),
        _ => class == CLASS_IN,
    }
}

struct Answer<'a> {
    records: &'a [Record],
    /// The lowest TTL of the CNAME records, which limits A and AAAA TTLs.
    cname_ttl: u32,
    any: bool,
}

impl<'a> Answer<'a> {
    fn matching(&self, kind: Kind) -> impl Iterator<Item = &'a Record> + use<'a> {
        let any = self.any;
        let wanted = move |record: &&Record| record.data.kind() == Some(kind) && class_matches(kind, record.class, any);
        self.records.iter().filter(wanted)
    }

    fn has(&self, kind: Kind) -> bool {
        self.matching(kind).next().is_some()
    }

    /// `ares_parse_a_reply` succeeds on an address of the asked family or a CNAME record.
    fn hosts(&self, kind: Kind) -> Result<(), &'static str> {
        if self.has(kind) || self.has(Kind::Cname) { Ok(()) } else { Err(NODATA) }
    }

    /// The first CNAME target when the answer has a CNAME record, else the question name
    /// as the reply spells it.
    fn canonical(&self, asked: &Name) -> String {
        let target = self.matching(Kind::Cname).find_map(|record| match &record.data {
            Data::Cname(target) => Some(target.text()),
            _ => None,
        });
        target.unwrap_or_else(|| asked.text())
    }

    /// The shape `resolve<Type>` returns for one record.
    fn single(&self, record: &Record, ttl: bool) -> Json {
        match &record.data {
            Data::A(_) | Data::Aaaa(_) if ttl => Json::Object(object(record, self.cname_ttl)),
            data => bare(data).unwrap_or_else(|| Json::Object(object(record, self.cname_ttl))),
        }
    }

    fn list(&self, kind: Kind, ttl: bool) -> Vec<Json> {
        self.matching(kind).map(|record| self.single(record, ttl)).collect()
    }

    /// Node 22 stores each TLSA object at the list length before the first one plus its
    /// answer-section index; the gaps are holes in Node and `null` here.
    fn tlsa_slots(&self, list: &mut Vec<Json>) {
        let offset = list.len();
        for (index, record) in self.records.iter().enumerate() {
            if record.data.kind() == Some(Kind::Tlsa) {
                list.resize(offset + index, Json::Null);
                list.push(Json::Object(object(record, self.cname_ttl)));
            }
        }
    }

    fn any(&self) -> Json {
        let tagged = |kind: Kind, record: &Record| {
            let mut object = object(record, self.cname_ttl);
            object.insert("type".into(), json!(kind.label()));
            Json::Object(object)
        };
        let mut list = Vec::new();
        match self.matching(Kind::Cname).next() {
            Some(record) => list.push(tagged(Kind::Cname, record)),
            None => list.extend(self.matching(Kind::A).map(|record| tagged(Kind::A, record))),
        }
        for kind in ANY_ORDER {
            let mut found = self.matching(kind).map(|record| tagged(kind, record));
            match kind {
                Kind::Soa => list.extend(found.next()),
                // Node 22 adds no `type` key to TLSA entries.
                Kind::Tlsa => self.tlsa_slots(&mut list),
                _ => list.extend(found),
            }
        }
        Json::Array(list)
    }
}

/// Formats the answer section of a reply that c-ares passes to Node's parser for `kind`.
/// `asked` is the question name in the reply. `Err` is the parser's status.
pub(super) fn reply(kind: Kind, asked: &Name, records: &[Record], ttl: bool) -> Result<Json, &'static str> {
    let kind = if kind == Kind::Reverse { Kind::Ptr } else { kind };
    let cnames = records.iter().filter(|record| matches!(record.data, Data::Cname(_)) && record.class == CLASS_IN);
    let cname_ttl = cnames.map(|record| record.ttl).min().unwrap_or(u32::MAX);
    let answer = Answer { records, cname_ttl, any: kind == Kind::Any };
    Ok(match kind {
        Kind::Any => answer.any(),
        Kind::A | Kind::Aaaa => {
            answer.hosts(kind)?;
            Json::Array(answer.list(kind, ttl))
        }
        Kind::Cname => {
            answer.hosts(Kind::A)?;
            json!([answer.canonical(asked)])
        }
        Kind::Soa => answer.matching(Kind::Soa).next().map(|record| answer.single(record, ttl)).ok_or(BADRESP)?,
        Kind::Tlsa => {
            let mut list = Vec::new();
            answer.tlsa_slots(&mut list);
            Json::Array(list)
        }
        // These parsers fail an empty answer section; NS and PTR also need a record.
        _ if records.is_empty() => return Err(NODATA),
        Kind::Ns | Kind::Ptr if !answer.has(kind) => return Err(NODATA),
        _ => Json::Array(answer.list(kind, ttl)),
    })
}
