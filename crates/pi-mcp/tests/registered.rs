//! Servers extensions register, overridden notices, connection ids, and subscriptions. Ported
//! from the registered-server tests of coding-agent's agent-session-mcp.test.ts.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::fake_mcp::{Fake, FakeOptions, Setup, entry, setup};
use common::wait_until;
use pi_mcp::runtime::TransportFactory;
use pi_mcp::{LoadedMcpConfig, McpConfigScope, McpServerRegistry, ToolExposure};
use serde_json::json;

type Connected = Arc<Mutex<Vec<(String, Option<McpConfigScope>)>>>;

/// A manager over fakes that records the name and scope of each server it connects.
fn with_registry(registry: &McpServerRegistry, configured: Vec<pi_mcp::McpServerEntry>) -> (Setup, Connected) {
    let fake = Fake::new(FakeOptions::default());
    let connected: Connected = Arc::default();
    let (seen, inner) = (connected.clone(), fake.factory());
    let factory: TransportFactory = Arc::new(move |entry, cwd, auth| {
        seen.lock().unwrap().push((entry.name.clone(), entry.scope));
        inner(entry, cwd, auth)
    });
    let registry = registry.clone();
    let loaded = LoadedMcpConfig { servers: configured, ..LoadedMcpConfig::default() };
    let setup = setup(fake, loaded, move |options| {
        options.create_transport = factory;
        options.registry = Some(registry);
    });
    (setup, connected)
}

fn names(connected: &Mutex<Vec<(String, Option<McpConfigScope>)>>) -> Vec<String> {
    connected.lock().unwrap().iter().map(|(name, _)| name.clone()).collect()
}

#[tokio::test]
async fn connects_servers_registered_while_extensions_load() {
    let registry = McpServerRegistry::new();
    registry
        .register("/ext/plugin.ts", "plugin", &json!({ "url": "http://plugin.invalid", "exposure": "direct" }))
        .unwrap();
    let (setup, connected) = with_registry(&registry, Vec::new());
    wait_until(|| setup.manager.tool("mcp__plugin__search").is_some()).await;
    assert_eq!(*connected.lock().unwrap(), [("plugin".to_string(), Some(McpConfigScope::Extension))]);
    let status = setup.manager.status("plugin").unwrap();
    assert_eq!(status.entry.source, "/ext/plugin.ts");
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn connects_and_disconnects_servers_registered_during_the_session() {
    let registry = McpServerRegistry::new();
    let changes = Arc::new(AtomicUsize::new(0));
    let (setup, connected) = with_registry(&registry, Vec::new());
    let counted = changes.clone();
    let subscription = setup.manager.subscribe(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    });

    registry.register("/ext/late.ts", "late", &json!({ "url": "http://late.invalid" })).unwrap();
    wait_until(|| setup.manager.tool("mcp__late__search").is_some()).await;
    assert_eq!(names(&connected), ["late"]);
    assert!(changes.load(Ordering::SeqCst) > 0);

    // Registering the same config again keeps the connection; another config replaces it.
    registry.register("/ext/late.ts", "late", &json!({ "url": "http://late.invalid" })).unwrap();
    registry.register("/ext/late.ts", "late", &json!({ "url": "http://late.invalid", "exposure": "direct" })).unwrap();
    let exposure = || setup.manager.tool("mcp__late__search").map(|tool| tool.exposure());
    wait_until(|| exposure() == Some(ToolExposure::Direct)).await;
    assert_eq!(names(&connected), ["late", "late"]);

    registry.unregister("late", "/ext/other.ts");
    assert!(registry.get("late").is_some());
    registry.unregister("late", "/ext/late.ts");
    wait_until(|| setup.manager.status("late").is_none()).await;
    assert_eq!(exposure(), Some(ToolExposure::Hidden));

    // Unsubscribed listeners are not called again.
    assert!(setup.manager.unsubscribe(subscription));
    assert!(!setup.manager.unsubscribe(subscription));
    let before = changes.load(Ordering::SeqCst);
    registry.register("/ext/late.ts", "again", &json!({ "url": "http://again.invalid" })).unwrap();
    wait_until(|| setup.manager.tool("mcp__again__search").is_some()).await;
    assert_eq!(changes.load(Ordering::SeqCst), before);
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn prefers_the_mcp_json_server_over_a_registered_one() {
    // "my_docs" shares the namespace of "my-docs".
    for name in ["my-docs", "my_docs"] {
        let registry = McpServerRegistry::new();
        registry.register("/ext/plugin.ts", name, &json!({ "url": "http://plugin.invalid" })).unwrap();
        let mut configured = entry("my-docs", json!({ "url": "http://config.invalid" }));
        configured.source = "mcp.json".into();
        let (setup, connected) = with_registry(&registry, vec![configured]);
        setup.manager.pending().await;
        assert_eq!(*connected.lock().unwrap(), [("my-docs".to_string(), None)]);
        let notice = format!("\"{name}\" registered by /ext/plugin.ts is overridden by \"my-docs\" in mcp.json");
        assert_eq!(setup.manager.overridden(), std::slice::from_ref(&notice));
        assert_eq!(setup.manager.notices(), [format!("overridden: {notice}")]);
        assert!(setup.manager.format_status().ends_with(&format!("\noverridden: {notice}")));
        setup.manager.shutdown().await;
    }
}

#[test]
fn rejects_names_another_extension_registered() {
    let registry = McpServerRegistry::new();
    registry.register("/ext/a.ts", "taken", &json!({ "url": "http://x.invalid" })).unwrap();
    // Registering again replaces the extension's own registration.
    registry.register("/ext/a.ts", "taken", &json!({ "url": "http://y.invalid" })).unwrap();
    registry.register("/ext/a.ts", "my-server", &json!({ "url": "http://x.invalid" })).unwrap();
    let error = registry.register("/ext/b.ts", "taken", &json!({ "url": "http://z.invalid" })).unwrap_err();
    assert_eq!(error, "MCP server \"taken\" is already registered by extension \"/ext/a.ts\"");
    let error = registry.register("/ext/b.ts", "my_server", &json!({ "url": "http://z.invalid" })).unwrap_err();
    assert_eq!(error, "MCP server \"my_server\" conflicts with registered server \"my-server\"");
    let error = registry.register("/ext/b.ts", "bad", &json!({ "exposure": "direct" })).unwrap_err();
    assert!(error.starts_with("Invalid MCP server registered by extension \"/ext/b.ts\": "), "{error}");
    let listed: Vec<_> = registry.list().into_iter().map(|server| (server.name, server.config.url)).collect();
    assert_eq!(
        listed,
        [("taken".into(), Some("http://y.invalid".into())), ("my-server".into(), Some("http://x.invalid".into()))]
    );
}

#[test]
fn reports_registered_servers_once_when_no_extension_connects_them() {
    let registry = McpServerRegistry::new();
    registry.register("/ext/orphan.ts", "orphan", &json!({ "url": "http://orphan.invalid" })).unwrap();
    let reports = registry.take_unconnected_reports();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].0, "/ext/orphan.ts");
    assert!(reports[0].1.starts_with("MCP server \"orphan\" is registered, but no loaded extension"));
    assert!(registry.take_unconnected_reports().is_empty());
}

#[tokio::test]
async fn drops_results_of_actions_for_a_replaced_connection() {
    let setup = setup(
        Fake::new(FakeOptions::default()),
        LoadedMcpConfig {
            servers: vec![entry("docs", json!({ "url": "http://unused.invalid" }))],
            ..Default::default()
        },
        |options| options.update_config = Some(Arc::new(|_, _| Ok(()))),
    );
    setup.manager.pending().await;
    let first = setup.manager.connection_id("docs").unwrap();
    assert!(setup.manager.set_message_if_current("docs", first, Some("kept".into())));
    assert_eq!(setup.manager.status("docs").unwrap().message.as_deref(), Some("kept"));

    // Disabling and enabling again replaces the connection the earlier result belonged to.
    assert_eq!(setup.manager.set_enabled("docs", false, false).await, None);
    let disabled = setup.manager.connection_id("docs").unwrap();
    assert_ne!(disabled, first);
    assert!(!setup.manager.set_message_if_current("docs", first, Some("stale".into())));
    assert_eq!(setup.manager.set_enabled("docs", true, false).await, None);
    let second = setup.manager.connection_id("docs").unwrap();
    assert_ne!(second, first);
    assert!(!setup.manager.set_message_if_current("docs", disabled, Some("stale".into())));
    assert_eq!(setup.manager.status("docs").unwrap().message.as_deref(), Some("kept"));
    assert!(setup.manager.set_message_if_current("docs", second, None));
    assert_eq!(setup.manager.status("docs").unwrap().message, None);

    setup.manager.shutdown().await;
    assert!(!setup.manager.set_message_if_current("docs", second, Some("late".into())));
}
