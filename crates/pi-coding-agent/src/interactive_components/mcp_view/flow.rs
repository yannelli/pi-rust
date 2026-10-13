//! The `/mcp` manager flow (upstream `manage`, `runAction`, and `signInWithUi`): pick a server,
//! then act on it until going back. Runs on the UI thread inside a `LocalSet`.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use async_trait::async_trait;
use pi_mcp::McpExposure;
use pi_mcp::runtime::McpSignInPrompt;
use reqwest::Url;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::servers::{exposure_menu, server_menu, servers_menu, tools_menu};
use super::{McpManagerView, McpMenuBuilder, McpServers};
use crate::interactive_components::login::BrowserOpenFn;

type View = Rc<RefCell<McpManagerView>>;

/// Shows a menu and waits for the chosen value; `None` when cancelled or replaced.
pub async fn show_menu(view: &View, build: McpMenuBuilder, live: bool) -> Option<String> {
    let (sender, receiver) = oneshot::channel();
    view.borrow_mut().menu(build, live, move |choice| {
        let _ = sender.send(choice);
    });
    receiver.await.ok().flatten()
}

/// Upstream `manage`: the servers menu, then each server's actions, until closed. Menus rebuild
/// while servers change; `open_browser` opens sign-in pages.
pub async fn run_mcp_manager(view: &View, servers: Rc<dyn McpServers>, open_browser: BrowserOpenFn) {
    let (changed, mut changes) = mpsc::unbounded_channel::<()>();
    let subscription = servers.subscribe(Box::new(move || {
        let _ = changed.send(());
    }));
    let weak = Rc::downgrade(view);
    let rebuilds = tokio::task::spawn_local(async move {
        while changes.recv().await.is_some() {
            let Some(view) = weak.upgrade() else { break };
            view.borrow_mut().rebuild_menu();
        }
    });
    loop {
        let list = servers.clone();
        let Some(name) = show_menu(view, Rc::new(move || servers_menu(&*list)), true).await else { break };
        loop {
            let (list, server) = (servers.clone(), name.clone());
            let action = show_menu(view, Rc::new(move || server_menu(&*list, &server)), true).await;
            let Some(action) = action.filter(|_| servers.status(&name).is_some()) else { break };
            run_action(view, &servers, &name, &action, &open_browser).await;
        }
    }
    rebuilds.abort();
    if let Some(subscription) = subscription {
        servers.unsubscribe(subscription);
    }
}

/// Runs `operation` without blocking the menu, then shows its message with the server unless
/// the connection it started for was disabled or replaced meanwhile (upstream `runInBackground`).
fn in_background(servers: &Rc<dyn McpServers>, name: &str, operation: impl Future<Output = Option<String>> + 'static) {
    let (servers, name) = (servers.clone(), name.to_string());
    tokio::task::spawn_local(async move {
        let mut operation = std::pin::pin!(operation);
        // The first poll starts the action, which may start a connection, before its id is read.
        let first = futures::poll!(operation.as_mut());
        let id = servers.connection_id(&name);
        let message = match first {
            std::task::Poll::Ready(message) => message,
            std::task::Poll::Pending => operation.await,
        };
        servers.set_message_if_current(&name, id, message);
    });
}

async fn run_action(view: &View, servers: &Rc<dyn McpServers>, name: &str, action: &str, open_browser: &BrowserOpenFn) {
    let mut message = None;
    match action {
        "signin" => message = sign_in_with_view(view, servers, name, open_browser).await,
        "reconnect" => {
            let (task, server) = (servers.clone(), name.to_string());
            // Connection state and error already report failures, including required sign-ins.
            in_background(servers, name, async move {
                task.reconnect(&server).await;
                None
            });
        }
        "signout" => {
            servers.sign_out(name).await;
        }
        "tools" => {
            if let Some(status) = servers.status(name) {
                let tools = servers.tools(name);
                show_menu(view, Rc::new(move || tools_menu(&status, &tools)), false).await;
            }
        }
        "exposure" => message = choose_exposure(view, servers, name).await,
        "enable" | "disable" | "enable-project" | "disable-project" => {
            let (task, server) = (servers.clone(), name.to_string());
            let (enable, in_project) = (action.starts_with("enable"), action.ends_with("-project"));
            in_background(servers, name, async move { task.set_enabled(&server, enable, in_project).await });
        }
        _ => {}
    }
    servers.set_message(name, message);
}

async fn choose_exposure(view: &View, servers: &Rc<dyn McpServers>, name: &str) -> Option<String> {
    let status = servers.status(name)?;
    let current = status.entry.config.exposure();
    let choice = show_menu(view, Rc::new(move || exposure_menu(&status)), false).await?;
    let exposure = McpExposure::parse(&choice).filter(|exposure| *exposure != current)?;
    servers.set_exposure(name, exposure)
}

enum PromptRequest {
    ShowUrl(String),
    Redirect(CancellationToken, oneshot::Sender<Option<String>>),
}

/// The `Send + Sync` sign-in prompt; a UI-thread task shows its requests in the view.
struct ViewSignInPrompt {
    sender: mpsc::UnboundedSender<PromptRequest>,
}

#[async_trait]
impl McpSignInPrompt for ViewSignInPrompt {
    fn show_authorization_url(&self, _server: &str, url: &Url) {
        let _ = self.sender.send(PromptRequest::ShowUrl(url.to_string()));
    }

    async fn prompt_for_redirect_url(&self, _server: &str, cancel: CancellationToken) -> Option<String> {
        let (reply, receiver) = oneshot::channel();
        self.sender.send(PromptRequest::Redirect(cancel, reply)).ok()?;
        receiver.await.ok().flatten()
    }
}

fn show_status(view: &Weak<RefCell<McpManagerView>>, title: &str, message: &str, cancel: &CancellationToken) {
    let Some(view) = view.upgrade() else { return };
    let cancel = cancel.clone();
    view.borrow_mut().status(title, message, Some(Box::new(move || cancel.cancel())));
}

/// Upstream `signInWithUi`: signs in to `name` with the view's status and redirect screens. The
/// cancel key stops it. Returns the failure message.
pub async fn sign_in_with_view(
    view: &View,
    servers: &Rc<dyn McpServers>,
    name: &str,
    open_browser: &BrowserOpenFn,
) -> Option<String> {
    let title = format!("Sign in to {name}");
    let cancel = CancellationToken::new();
    let weak = Rc::downgrade(view);
    show_status(&weak, &title, "Contacting the authorization server…", &cancel);
    let (sender, mut requests) = mpsc::unbounded_channel();
    let (open_browser, token) = (open_browser.clone(), cancel.clone());
    let prompts = tokio::task::spawn_local(async move {
        let mut authorization_url = String::new();
        while let Some(request) = requests.recv().await {
            match request {
                PromptRequest::ShowUrl(url) => {
                    open_browser(&url);
                    authorization_url = url;
                }
                PromptRequest::Redirect(arrived, reply) => {
                    let Some(view) = weak.upgrade() else { break };
                    let (done, value) = oneshot::channel();
                    if arrived.is_cancelled() {
                        let _ = done.send(None);
                    } else {
                        view.borrow_mut().redirect_url(&title, &authorization_url, move |url| {
                            let _ = done.send(url);
                        });
                    }
                    drop(view);
                    let url = tokio::select! {
                        url = value => url.ok().flatten(),
                        () = arrived.cancelled() => None,
                    };
                    show_status(&weak, &title, "Connecting…", &token);
                    let _ = reply.send(url);
                }
            }
        }
    });
    let failure = servers.sign_in(name, Arc::new(ViewSignInPrompt { sender }), cancel).await;
    prompts.abort();
    failure
}
