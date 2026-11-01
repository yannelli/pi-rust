//! Ports `mcp-manager-view.test.ts` and covers the `/mcp` menus, the sign-in screen, and the
//! manager flow over a fake [`McpServers`].

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pi_mcp::runtime::{McpSignInPrompt, ServerState};
use pi_mcp::{McpConfigScope, McpExposure, McpServerConfig, McpServerEntry, McpServerStatus, McpSubscription, Tool};
use pi_tui::Component;
use pi_tui::components::SelectItem;
use reqwest::Url;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::interactive_components::login::{BrowserOpenFn, ClipboardCopyFn};
use crate::interactive_components::mcp_view::{
    McpManagerView, McpMenu, McpServers, exposure_menu, run_mcp_manager, server_menu, servers_menu, tools_menu,
};
use crate::interactive_components::test_support::{Calls, Harness, flush, keys, local};

type View = Harness<McpManagerView>;

fn view() -> View {
    Harness::new_shared(100, 40, |tui| Rc::new(RefCell::new(McpManagerView::new(tui))))
}

/// Waits until one screen line contains every part.
async fn wait_line(harness: &View, parts: &[&str]) {
    for _ in 0..200 {
        flush(harness).await;
        if harness.viewport().iter().any(|line| parts.iter().all(|part| line.contains(part))) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {parts:?}:\n{}", harness.screen());
}

async fn wait_for(harness: &View, needle: &str) {
    wait_line(harness, &[needle]).await;
}

fn status(name: &str, config: McpServerConfig, scope: McpConfigScope, state: Option<ServerState>) -> McpServerStatus {
    let mut entry = McpServerEntry::new(name, config, format!("/cfg/{scope}/mcp.json"));
    entry.scope = Some(scope);
    let enabled = entry.config.is_enabled();
    McpServerStatus {
        entry,
        enabled,
        state,
        error: None,
        tools: Vec::new(),
        has_resources: false,
        oauth: false,
        message: None,
    }
}

fn sample() -> Vec<McpServerStatus> {
    let mut issues = status(
        "issues",
        McpServerConfig::http("https://mcp.example/issues"),
        McpConfigScope::Global,
        Some(ServerState::NeedsAuth),
    );
    issues.oauth = true;
    let mut config = McpServerConfig::stdio("docs-server", vec!["--stdio".into()]);
    config.tool_exposure = json!({"fetch": "direct"}).as_object().cloned();
    let mut docs = status("docs", config, McpConfigScope::Project, Some(ServerState::Connected));
    (docs.tools, docs.has_resources) = (vec!["search".into(), "fetch".into()], true);
    let mut config = McpServerConfig::http("https://old.example");
    config.enabled = Some(false);
    let old = status("old", config, McpConfigScope::Global, None);
    vec![issues, docs, old]
}

fn tool(name: &str, description: &str) -> Tool {
    Tool { description: Some(description.into()), ..Tool::new(name, json!({"type": "object"})) }
}

#[derive(Default)]
struct FakeServers {
    statuses: RefCell<Vec<McpServerStatus>>,
    errors: Vec<String>,
    project: bool,
    listeners: RefCell<Vec<Box<dyn Fn() + Send + Sync>>>,
    log: Calls<String>,
}

impl FakeServers {
    fn new(project: bool) -> Rc<Self> {
        Rc::new(Self { statuses: RefCell::new(sample()), project, ..Self::default() })
    }

    fn update(&self, name: &str, change: impl FnOnce(&mut McpServerStatus)) {
        if let Some(status) = self.statuses.borrow_mut().iter_mut().find(|status| status.entry.name == name) {
            change(status);
        }
        for listener in self.listeners.borrow().iter() {
            listener();
        }
    }
}

#[async_trait(?Send)]
impl McpServers for FakeServers {
    fn statuses(&self) -> Vec<McpServerStatus> {
        self.statuses.borrow().clone()
    }

    fn config_errors(&self) -> Vec<String> {
        self.errors.clone()
    }

    fn tools(&self, _name: &str) -> Vec<Tool> {
        vec![tool("search", "Search the docs\nwith filters"), tool("fetch", "Fetch a page")]
    }

    fn can_save_in_project(&self, name: &str) -> bool {
        let global = self.status(name).is_some_and(|status| status.entry.scope == Some(McpConfigScope::Global));
        self.project && global
    }

    fn empty_message(&self) -> String {
        "No MCP servers configured. Add them to /agent/mcp.json or .pi/mcp.json.".into()
    }

    fn subscribe(&self, listener: Box<dyn Fn() + Send + Sync>) -> Option<McpSubscription> {
        self.listeners.borrow_mut().push(listener);
        None
    }

    async fn sign_in(&self, name: &str, prompt: Arc<dyn McpSignInPrompt>, cancel: CancellationToken) -> Option<String> {
        prompt.show_authorization_url(name, &Url::parse("https://auth.example/authorize?client=pi").unwrap());
        let url = tokio::select! {
            url = prompt.prompt_for_redirect_url(name, CancellationToken::new()) => url,
            () = cancel.cancelled() => return Some("Sign-in cancelled.".into()),
        };
        self.log.push(format!("signin {name} {url:?}"));
        let Some(_) = url else { return Some("Sign-in cancelled.".into()) };
        self.update(name, |status| status.state = Some(ServerState::Connected));
        None
    }

    async fn sign_out(&self, name: &str) -> bool {
        self.log.push(format!("signout {name}"));
        true
    }

    async fn reconnect(&self, name: &str) -> Option<String> {
        self.log.push(format!("reconnect {name}"));
        Some("ignored".into())
    }

    async fn set_enabled(&self, name: &str, enabled: bool, in_project: bool) -> Option<String> {
        self.log.push(format!("enabled {name} {enabled} {in_project}"));
        self.update(name, |status| status.enabled = enabled);
        None
    }

    fn set_exposure(&self, name: &str, exposure: McpExposure) -> Option<String> {
        self.log.push(format!("exposure {name} {exposure}"));
        self.update(name, |status| status.entry.config.exposure = Some(exposure));
        None
    }

    fn set_message(&self, name: &str, message: Option<String>) {
        self.update(name, |status| status.message = message);
    }
}

fn labels(menu: &McpMenu) -> Vec<(String, Option<String>)> {
    menu.items.iter().map(|item| (item.label.clone(), item.description.clone())).collect()
}

fn pair(label: &str, description: Option<&str>) -> (String, Option<String>) {
    (label.to_string(), description.map(String::from))
}

#[test]
fn status_cancels_the_running_operation_with_the_cancel_key() {
    let harness = view();
    let cancels = Calls::new();
    let on_cancel = Box::new(cancels.counter());
    harness.component.borrow_mut().status("Sign in to issues", "Contacting the authorization server…", Some(on_cancel));
    harness.settle();
    assert!(harness.screen().contains("cancel"));
    harness.send(keys::ESCAPE);
    assert_eq!(cancels.len(), 1);

    harness.component.borrow_mut().status("Sign in to issues", "Connecting…", None);
    harness.settle();
    assert!(!harness.screen().contains("cancel") && harness.screen().contains(" Connecting…"));
    harness.send(keys::ESCAPE);
}

#[test]
fn captures_fullscreen_viewport_keys_only_while_a_menu_list_is_shown() {
    let harness = view();
    let choices = Calls::new();
    let items = (0..20).map(|i| SelectItem::new(&format!("server-{i}"), &format!("server-{i}"))).collect();
    let menu = McpMenu {
        title: "MCP servers".into(),
        items,
        confirm_label: "open".into(),
        cancel_label: "close".into(),
        ..McpMenu::default()
    };
    harness.component.borrow_mut().menu(Rc::new(move || menu.clone()), false, choices.recorder());
    harness.settle();
    assert!(harness.component.borrow().captures_viewport_input());

    harness.send_all(&[keys::PAGE_DOWN, keys::ENTER]);
    assert_eq!(choices.take(), [Some("server-12".to_string())]);

    harness.component.borrow_mut().status("Sign in to issues", "Connecting…", None);
    assert!(!harness.component.borrow().captures_viewport_input());
}

#[test]
fn menus_describe_servers_actions_tools_and_exposures() {
    let servers = FakeServers::new(true);
    let menu = servers_menu(&*servers);
    assert_eq!(
        labels(&menu),
        [
            pair("issues", Some("needs sign-in · codemode · global")),
            pair("docs", Some("connected · 2 tools · resources · codemode · project")),
            pair("old", Some("disabled · codemode · global")),
        ]
    );
    assert_eq!((menu.confirm_label.as_str(), menu.cancel_label.as_str(), menu.error), ("manage", "close", None));
    let failing = FakeServers { errors: vec!["bad json".into()], ..FakeServers::default() };
    let menu = servers_menu(&failing);
    assert_eq!(menu.error.as_deref(), Some("config: bad json"));
    assert!(menu.items.is_empty() && menu.empty.unwrap().starts_with("No MCP servers configured."));

    let menu = server_menu(&*servers, "docs");
    assert_eq!(menu.title, "MCP server docs");
    assert_eq!(
        menu.details.as_deref(),
        Some("docs-server --stdio\nproject: /cfg/project/mcp.json\nState: connected · 2 tools · resources")
    );
    let saved = Some("saved to the project mcp.json");
    let expected = [
        pair("Tools", Some("2 offered")),
        pair("Reconnect", None),
        pair("Exposure", Some("codemode")),
        pair("Disable", saved),
    ];
    assert_eq!(labels(&menu), expected);
    assert_eq!(menu.selected.as_deref(), Some("tools"));
    let menu = server_menu(&*servers, "issues");
    let global = Some("saved to the global mcp.json");
    let expected = [
        pair("Sign in", Some("opens the browser")),
        pair("Reconnect", None),
        pair("Exposure", Some("codemode")),
        pair("Disable", global),
        pair("Disable in this project", saved),
    ];
    assert_eq!(labels(&menu), expected);
    servers.update("old", |status| status.message = Some("Could not update /cfg: denied".into()));
    let menu = server_menu(&*servers, "old");
    assert_eq!(labels(&menu), [pair("Enable", global), pair("Enable in this project", saved)]);
    assert_eq!(menu.error.as_deref(), Some("Could not update /cfg: denied"));
    let gone = server_menu(&*servers, "gone");
    assert!(gone.items.is_empty() && gone.empty.as_deref() == Some("This server is no longer configured."));

    let docs = servers.status("docs").unwrap();
    let menu = tools_menu(&docs, &servers.tools("docs"));
    let details = "Exposure codemode: called from codemode scripts, which find them with searchTools()\n\
                   Some tools override it with toolExposure.";
    assert_eq!(menu.details.as_deref(), Some(details));
    assert_eq!(labels(&menu), [pair("search", Some("Search the docs")), pair("fetch", Some("[direct] Fetch a page"))]);
    let menu = exposure_menu(&docs);
    assert_eq!(menu.details.as_deref(), Some("Saved to /cfg/project/mcp.json."));
    assert_eq!(
        menu.items.iter().map(|item| item.label.as_str()).collect::<Vec<_>>(),
        ["✓ codemode", "  deferred", "  direct"]
    );
    assert_eq!(menu.selected.as_deref(), Some("codemode"));
}

#[test]
fn live_menus_rebuild_and_keep_the_selection() {
    let harness = view();
    let servers = FakeServers::new(false);
    let choices = Calls::new();
    let list = servers.clone();
    harness.component.borrow_mut().menu(Rc::new(move || servers_menu(&*list)), true, choices.recorder());
    harness.send(keys::DOWN);
    let screen = harness.screen();
    assert!(screen.contains(" MCP servers") && screen.contains("→ docs") && screen.contains("manage"), "{screen}");
    servers.statuses.borrow_mut()[1].state = Some(ServerState::Failed);
    servers.statuses.borrow_mut()[1].error = Some("spawn failed\nmore".into());
    harness.component.borrow_mut().rebuild_menu();
    harness.settle();
    assert!(harness.screen().contains("→ docs") && harness.screen().contains("failed: spawn failed"));
    harness.send(keys::ENTER);
    assert_eq!(choices.take(), [Some("docs".to_string())]);

    let empty = Rc::new(|| McpMenu { title: "Tools of x".into(), cancel_label: "back".into(), ..McpMenu::default() });
    harness.component.borrow_mut().menu(empty, false, choices.recorder());
    harness.settle();
    assert!(harness.screen().contains(" Nothing to show.") && harness.screen().contains("back"));
    harness.send_all(&[keys::ENTER, keys::ESCAPE, keys::ESCAPE]);
    assert_eq!(choices.take(), [None]);
}

#[tokio::test(flavor = "current_thread")]
async fn sign_in_screen_copies_and_takes_a_pasted_url() {
    local(async {
        let harness = view();
        let copied = Calls::new();
        let recorder = copied.clone();
        let copier: ClipboardCopyFn = Rc::new(move |text: String| {
            recorder.push(text);
            Box::pin(async { Ok(()) })
        });
        harness.component.borrow_mut().set_clipboard(copier);
        let done = Calls::new();
        harness.component.borrow_mut().redirect_url("Sign in to issues", "https://auth.example/a", done.recorder());
        harness.settle();
        let screen = harness.screen();
        assert!(screen.contains("Approve access in your browser. If it did not open, visit:"), "{screen}");
        assert!(screen.contains("https://auth.example/a") && screen.contains("paste the URL it was redirected to:"));
        assert!(screen.contains("submit") && screen.contains("cancel"));
        harness.send(&keys::ctrl('x'));
        flush(&harness).await;
        assert_eq!(copied.get(), ["https://auth.example/a"]);
        assert!(harness.screen().contains("Copied URL to clipboard"));
        harness.send_all(&[keys::SPACE, keys::ENTER]);
        assert!(done.is_empty(), "blank input is ignored");
        harness.type_text("http://127.0.0.1/callback?code=1 ");
        harness.send_all(&[keys::ENTER, keys::ESCAPE]);
        assert_eq!(done.take(), [Some("http://127.0.0.1/callback?code=1".to_string())]);
    })
    .await;
}

fn browser(opened: &Calls<String>) -> BrowserOpenFn {
    let opened = opened.clone();
    Rc::new(move |url: &str| opened.push(url.to_string()))
}

#[tokio::test(flavor = "current_thread")]
async fn manager_flow_changes_exposure_lists_tools_and_disables() {
    local(async {
        let harness = view();
        let servers = FakeServers::new(false);
        let finished = Rc::new(RefCell::new(false));
        let (view, flag, api) = (harness.component.clone(), finished.clone(), servers.clone());
        tokio::task::spawn_local(async move {
            run_mcp_manager(&view, api, browser(&Calls::new())).await;
            *flag.borrow_mut() = true;
        });
        wait_for(&harness, "→ issues").await;
        harness.send_all(&[keys::DOWN, keys::ENTER]);
        wait_for(&harness, "MCP server docs").await;
        assert!(harness.screen().contains("docs-server --stdio") && harness.screen().contains("→ Tools"));
        harness.send(keys::ENTER);
        wait_for(&harness, "Tools of docs").await;
        assert!(harness.screen().contains("[direct] Fetch a page"));
        harness.send(keys::ESCAPE);
        wait_for(&harness, "→ Tools").await;
        harness.send_all(&[keys::DOWN, keys::DOWN, keys::ENTER]);
        wait_for(&harness, "Exposure of docs").await;
        assert!(harness.screen().contains("→ ✓ codemode"));
        harness.send_all(&[keys::DOWN, keys::ENTER]);
        wait_line(&harness, &["Exposure", "deferred"]).await;
        harness.send_all(&[keys::DOWN, keys::DOWN, keys::DOWN, keys::ENTER]);
        wait_for(&harness, "→ Enable").await;
        assert_eq!(servers.log.get(), ["exposure docs deferred", "enabled docs false false"]);
        harness.send(keys::ESCAPE);
        wait_for(&harness, "disabled · deferred · project").await;
        servers.update("issues", |status| status.state = Some(ServerState::Failed));
        wait_for(&harness, "failed: unknown error").await;
        harness.send(keys::ESCAPE);
        flush(&harness).await;
        assert!(*finished.borrow());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn signing_in_opens_the_browser_and_reports_cancellation() {
    local(async {
        let harness = view();
        let servers = FakeServers::new(false);
        let opened = Calls::new();
        let (view, api, open) = (harness.component.clone(), servers.clone(), browser(&opened));
        tokio::task::spawn_local(async move { run_mcp_manager(&view, api, open).await });
        wait_for(&harness, "→ issues").await;
        harness.send(keys::ENTER);
        wait_for(&harness, "→ Sign in").await;
        harness.send(keys::ENTER);
        wait_for(&harness, "Sign in to issues").await;
        wait_for(&harness, "Approve access").await;
        assert_eq!(opened.get(), ["https://auth.example/authorize?client=pi"]);
        harness.send(keys::ESCAPE);
        wait_for(&harness, "Sign-in cancelled.").await;
        assert!(harness.screen().contains("MCP server issues"));
        harness.send(keys::ENTER);
        wait_for(&harness, "Approve access").await;
        harness.type_text("http://127.0.0.1/callback?code=2");
        harness.send(keys::ENTER);
        wait_for(&harness, "→ Tools").await;
        assert!(!harness.screen().contains("Sign-in cancelled."));
        assert_eq!(
            servers.log.get(),
            ["signin issues None", "signin issues Some(\"http://127.0.0.1/callback?code=2\")"]
        );
    })
    .await;
}
