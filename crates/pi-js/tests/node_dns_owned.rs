//! The resolve family against loopback DNS servers matches real Node 22. Each case is a script body plus
//! the JSON Node 22 printed for it; `node fixtures/node_dns_owned/oracle.cjs <case>.js <case>.json` rechecks one.

use crate::common;
use crate::node_common;

use std::path::Path;

use common::*;
use serde_json::{Value, json};

const SERVER: &str = include_str!("fixtures/node_dns_owned/dns_server.cjs");

fn bindable(address: &str) -> bool {
    std::net::UdpSocket::bind(address).is_ok() && std::net::TcpListener::bind(address).is_ok()
}

/// The golden for a case, adjusted for addresses this host does not have.
fn expected(name: &str, golden: &str) -> Value {
    let mut expected: Value = serde_json::from_str(golden).unwrap();
    let object = expected.as_object_mut().unwrap();
    if name == "transport" {
        if !bindable("[::1]:0") {
            object.remove("v6Servers");
            object.remove("v6Answer");
        }
        if !bindable("127.0.0.2:0") {
            object.remove("boundSecond");
        }
    }
    expected
}

/// Every key that differs, so one run reports them all.
fn differences(actual: &Value, expected: &Value) -> Vec<String> {
    let (Some(actual), Some(expected)) = (actual.as_object(), expected.as_object()) else {
        return vec![format!("not objects: {actual} vs {expected}")];
    };
    let mut found: Vec<String> = expected
        .iter()
        .filter(|(key, value)| actual.get(*key) != Some(*value))
        .map(|(key, value)| format!("{key}: got {}, Node {value}", actual.get(key).unwrap_or(&Value::Null)))
        .collect();
    found.extend(actual.keys().filter(|key| !expected.contains_key(*key)).map(|key| format!("{key}: not in Node")));
    found
}

async fn run_case(runtime: &pi_js::JsRuntime, root: &Path, body: &str) -> Value {
    write(root, "dns_server.cjs", SERVER);
    run_cjs_within(runtime, root, body, 60).await
}

async fn quickjs_case(name: &str, body: &str, golden: &str) {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let found = differences(&run_case(&runtime, &root, body).await, &expected(name, golden));
    assert!(found.is_empty(), "{name} differs from Node 22:\n{}", found.join("\n"));
    assert!(capture.uncaught.lock().unwrap().is_empty(), "{name}: {:?}", capture.uncaught.lock().unwrap());
}

macro_rules! case {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!("fixtures/node_dns_owned/", $name, ".js")),
            include_str!(concat!("fixtures/node_dns_owned/", $name, ".json")),
        )
    };
}

const CASES: [(&str, &str, &str); 5] =
    [case!("records"), case!("errors"), case!("transport"), case!("cancel"), case!("compat")];

#[tokio::test]
async fn dns_record_shapes_match_node() {
    let (name, body, golden) = CASES[0];
    quickjs_case(name, body, golden).await;
}

#[tokio::test]
async fn dns_errors_and_validation_match_node() {
    let (name, body, golden) = CASES[1];
    quickjs_case(name, body, golden).await;
}

#[tokio::test]
async fn dns_tcp_fallback_binding_and_failover_match_node() {
    let (name, body, golden) = CASES[2];
    quickjs_case(name, body, golden).await;
}

#[tokio::test]
async fn dns_cancel_matches_node() {
    let (name, body, golden) = CASES[3];
    quickjs_case(name, body, golden).await;
}

#[tokio::test]
async fn dns_aliases_server_selection_and_tcp_match_node() {
    let (name, body, golden) = CASES[4];
    quickjs_case(name, body, golden).await;
}

#[tokio::test]
async fn reserved_names_and_invalid_idna_stay_local() {
    let (_dir, root) = tempdir();
    let (runtime, _) = runtime(&root);
    let script = r#"
        const { Resolver } = require("node:dns/promises");
        const { start } = require("./dns_server.cjs");
        const mark = text => require("node:fs").appendFileSync(require("node:path").join(__dirname, "stage"), text + "\n");
        const server = await start(() => ({ answers: [["@", "A", 60, "192.0.2.17"]] }));
        try {
          const r = new Resolver({ tries: 1, timeout: 1000 });
          r.setServers([server.address]);
          const result = p => p.catch(error => error.code);
          mark("local answers");
          const pending = r.resolve4("api.LOCALHOST.", { ttl: true });
          r.cancel();
          const local = [await pending, await r.resolve6("localhost"), await r.resolvePtr("12.0.0.127.in-addr.arpa"),
            await result(r.resolveMx("localhost")), await result(r.resolve4("onion")),
            await result(r.resolve4(String.raw`host.\111nion`)), await result(r.resolve4("host.invalid")),
            await result(r.resolve4("\uFFFD"))];
          mark("ordinary DNS");
          const remote = [await r.resolve4("."), await r.resolve4("notlocalhost.test")];
          const queries = server.log.filter(entry => entry.type === "A").map(entry => entry.name);
          r.setServers([]);
          return [local, remote, queries, await r.resolve4("localhost"), await result(r.resolve4("ordinary.test"))];
        } finally { await server.close(); }
    "#;
    assert_eq!(
        run_case(&runtime, &root, script).await,
        json!([
            [[{"address": "127.0.0.1", "ttl": 86400}], ["::1"], ["localhost"], "ENODATA",
             "ENOTFOUND", "ENOTFOUND", "ENOTFOUND", "EBADNAME"],
            [["192.0.2.17"], ["192.0.2.17"]], ["", "notlocalhost.test"], ["127.0.0.1"], "UNKNOWN_ARES_ERROR"
        ])
    );
}

/// Reruns every case on real Node through the Node backend, so CI hosts (Windows included) recheck the
/// goldens. Skips without Node 22, since other majors bundle other c-ares releases.
#[tokio::test]
async fn dns_fixtures_match_the_node_backend() {
    let (_dir, root) = tempdir();
    let capture = std::sync::Arc::new(Capture::default());
    let Some(runtime) = node_common::node_runtime_with(options(&root, &capture)) else { return };
    let probe = r#"return [process.version, typeof require("node:dns").resolveTlsa];"#;
    let probe = run_cjs_within(&runtime, &root, probe, 30).await;
    let version = probe[0].as_str().unwrap_or_default().to_string();
    if !version.starts_with("v22.") || probe[1] != "function" {
        eprintln!(
            "skipping: the DNS goldens need Node 22.15 or later 22.x (resolveTlsa); the Node backend runs {version}"
        );
        return;
    }
    let (quickjs, _) = common::runtime(&root);
    let servers = r#"return new (require("node:dns").Resolver)().getServers();"#;
    let system = run_cjs_within(&quickjs, &root, servers, 30).await;
    let node_system = run_cjs_within(&runtime, &root, servers, 30).await;
    if system == json!([]) {
        assert!(node_system == json!([]) || node_system == json!(["127.0.0.1"]), "system DNS servers: {node_system}");
    } else {
        assert_eq!(system, node_system, "system DNS servers differ from Node {version}");
    }
    let reverse = r#"
        const { start } = require("./dns_server.cjs");
        const server = await start(() => ({answers: [["@", "PTR", 60, "from-dns.test"]]}));
        try {
          const resolver = new (require("node:dns/promises").Resolver)({timeout: 1000, tries: 1});
          resolver.setServers([server.address]);
          const rows = [];
          for (const host of ["127.0.0.1", "::1", "192.0.2.243"]) {
            const before = server.log.length;
            const names = await resolver.reverse(host).catch(error => error.code);
            rows.push([names, server.log.slice(before).filter(entry => entry.type === "PTR").length]);
          }
          const pending = resolver.reverse("127.0.0.1").catch(error => error.code);
          let changed = true;
          try { resolver.setServers([server.address]); } catch (error) { changed = error.code; }
          resolver.cancel();
          rows.push([changed, await pending]);
          return rows;
        } finally { await server.close(); }
    "#;
    let mut expected_reverse = run_case(&runtime, &root, reverse).await;
    for index in 0..2 {
        if expected_reverse[index] == json!([["from-dns.test"], 1]) {
            // QuickJS answers a loopback PTR locally when hosts-file policy would otherwise send it to DNS.
            expected_reverse[index] = json!([["localhost"], 0]);
            if index == 0 {
                expected_reverse[3] = json!([true, ["localhost"]]);
            }
        }
    }
    assert_eq!(run_case(&quickjs, &root, reverse).await, expected_reverse, "reverse lookup on Node {version}");
    let mut failures = Vec::new();
    for (name, body, golden) in CASES {
        let actual = run_case(&runtime, &root, body).await;
        failures.extend(differences(&actual, &expected(name, golden)).into_iter().map(|line| format!("{name}.{line}")));
    }
    assert!(failures.is_empty(), "Node {version} differs from the goldens:\n{}", failures.join("\n"));
}
