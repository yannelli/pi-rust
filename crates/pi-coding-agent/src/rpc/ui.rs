//! The extension UI over RPC (upstream `createExtensionUIContext` in `rpc-mode.ts` and
//! `docs/rpc-extension-ui.md`): dialogs become `extension_ui_request` records answered by
//! `extension_ui_response`; the other methods are fire-and-forget records.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::{oneshot, watch};

use super::output::RpcOutput;
use crate::agent_session::lock;
use crate::extension_ui::{DialogOptions, ExtensionUi, NotifyType, WidgetPlacement};
use crate::session::ids::uuid_v4;

type Response = Map<String, Value>;

/// [`ExtensionUi`] for RPC mode. Unanswered dialogs wait until their timeout or cancellation.
pub struct RpcExtensionUi {
    output: RpcOutput,
    pending: Mutex<HashMap<String, oneshot::Sender<Response>>>,
    /// The number of dialogs that wait for the client, sent or held.
    waiting: watch::Sender<usize>,
    /// False while dialogs are held until [`Self::open_dialogs`].
    open: watch::Sender<bool>,
}

fn request(method: &str, fields: Value) -> Map<String, Value> {
    let mut record = Map::new();
    record.insert("type".into(), json!("extension_ui_request"));
    record.insert("id".into(), json!(uuid_v4()));
    record.insert("method".into(), json!(method));
    if let Value::Object(fields) = fields {
        record.extend(fields.into_iter().filter(|(_, value)| !value.is_null()));
    }
    record
}

fn timeout_ms(opts: &DialogOptions) -> Value {
    opts.timeout.map_or(Value::Null, |timeout| json!(timeout.as_millis() as u64))
}

/// Upstream checks `cancelled` for JavaScript truthiness.
fn is_cancelled(response: &Response) -> bool {
    match response.get("cancelled") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

/// `value` of a select, input, or editor response; `None` when cancelled.
fn response_value(response: Option<Response>) -> Option<String> {
    let response = response.filter(|response| !is_cancelled(response))?;
    match response.get("value")? {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

impl RpcExtensionUi {
    pub fn new(output: RpcOutput) -> Arc<Self> {
        Self::with_dialogs_open(output, true)
    }

    /// [`Self::new`], but dialogs wait for [`Self::open_dialogs`] before they are sent.
    pub(crate) fn held(output: RpcOutput) -> Arc<Self> {
        Self::with_dialogs_open(output, false)
    }

    fn with_dialogs_open(output: RpcOutput, open: bool) -> Arc<Self> {
        Arc::new(Self {
            output,
            pending: Mutex::new(HashMap::new()),
            waiting: watch::channel(0).0,
            open: watch::channel(open).0,
        })
    }

    /// Send the held dialogs, and later dialogs at once.
    pub(crate) fn open_dialogs(&self) {
        self.open.send_replace(true);
    }

    /// Wait until a dialog waits for the client.
    pub(crate) async fn wait_for_dialog(&self) {
        let _ = self.waiting.subscribe().wait_for(|count| *count > 0).await;
    }

    /// Deliver an `extension_ui_response`; false when no dialog waits for `id`.
    pub fn resolve(&self, id: &str, response: Response) -> bool {
        match self.take_pending(id) {
            Some(sender) => sender.send(response).is_ok(),
            None => false,
        }
    }

    fn take_pending(&self, id: &str) -> Option<oneshot::Sender<Response>> {
        let mut pending = lock(&self.pending);
        let sender = pending.remove(id);
        self.waiting.send_replace(pending.len());
        sender
    }

    /// Dialogs waiting for a response.
    pub fn pending_count(&self) -> usize {
        lock(&self.pending).len()
    }

    /// Send a dialog request and wait for its response; `None` on timeout or cancellation. A
    /// held dialog is sent when dialogs open, and its timeout starts then.
    async fn dialog(&self, opts: DialogOptions, record: Map<String, Value>) -> Option<Response> {
        let cancel = opts.cancel.unwrap_or_default();
        if cancel.is_cancelled() {
            return None;
        }
        let id = record["id"].as_str().unwrap_or_default().to_string();
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = lock(&self.pending);
            pending.insert(id.clone(), sender);
            self.waiting.send_replace(pending.len());
        }
        let answer = async {
            let _ = self.open.subscribe().wait_for(|open| *open).await;
            self.output.send(&Value::Object(record));
            let timeout = async {
                match opts.timeout {
                    Some(timeout) => tokio::time::sleep(timeout).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                response = receiver => response.ok(),
                _ = timeout => None,
            }
        };
        let response = tokio::select! {
            response = answer => response,
            _ = cancel.cancelled() => None,
        };
        self.take_pending(&id);
        response
    }

    fn fire(&self, record: Map<String, Value>) {
        self.output.send(&Value::Object(record));
    }
}

#[async_trait]
impl ExtensionUi for RpcExtensionUi {
    async fn select(&self, title: &str, options: &[String], opts: DialogOptions) -> Option<String> {
        let record = request("select", json!({"title": title, "options": options, "timeout": timeout_ms(&opts)}));
        response_value(self.dialog(opts, record).await)
    }

    async fn confirm(&self, title: &str, message: &str, opts: DialogOptions) -> bool {
        let record = request("confirm", json!({"title": title, "message": message, "timeout": timeout_ms(&opts)}));
        let response = self.dialog(opts, record).await.filter(|response| !is_cancelled(response));
        response.and_then(|response| response.get("confirmed").and_then(Value::as_bool)).unwrap_or(false)
    }

    async fn input(&self, title: &str, placeholder: Option<&str>, opts: DialogOptions) -> Option<String> {
        let mut fields = json!({"title": title, "placeholder": placeholder, "timeout": timeout_ms(&opts)});
        if let Some(prefill) = &opts.prefill {
            fields["prefill"] = json!(prefill);
        }
        if opts.secret {
            fields["secret"] = json!(true);
        }
        response_value(self.dialog(opts, request("input", fields)).await)
    }

    async fn editor(&self, title: &str, prefill: Option<&str>, opts: DialogOptions) -> Option<String> {
        let fields = json!({"title": title, "prefill": prefill, "timeout": timeout_ms(&opts)});
        response_value(self.dialog(opts, request("editor", fields)).await)
    }

    fn notify(&self, message: &str, kind: Option<NotifyType>) {
        self.fire(request("notify", json!({"message": message, "notifyType": kind.map(NotifyType::as_str)})));
    }

    fn set_status(&self, key: &str, text: Option<&str>) {
        self.fire(request("setStatus", json!({"statusKey": key, "statusText": text})));
    }

    fn set_widget(&self, key: &str, lines: Option<&[String]>, placement: Option<WidgetPlacement>) {
        let fields =
            json!({"widgetKey": key, "widgetLines": lines, "widgetPlacement": placement.map(WidgetPlacement::as_str)});
        self.fire(request("setWidget", fields));
    }

    fn set_title(&self, title: &str) {
        self.fire(request("setTitle", json!({"title": title})));
    }

    fn set_editor_text(&self, text: &str) {
        self.fire(request("set_editor_text", json!({"text": text})));
    }

    fn set_theme(&self, _name: &str) -> Result<(), String> {
        Err("Theme switching not supported in RPC mode".into())
    }
}
