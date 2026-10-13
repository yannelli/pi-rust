//! The native half of `fetch`: a reqwest request whose response head, body chunks, and end or
//! error are posted back to a JavaScript callback.

use std::sync::OnceLock;
use std::time::Duration;

use futures::StreamExt;
use rquickjs::{Ctx, Function, Object, Persistent, Value};
use serde::Deserialize;

use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{bytes_to_js, error_value, latin1_to_bytes, throw, to_json};
use crate::runtime::state::{Message, Task, state};
use crate::runtime::{UncaughtKind, report_error};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FetchRequest {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    /// `follow`, `manual`, or `error`.
    redirect: String,
}

/// HTTP clients for one runtime, created on first use. Redirect policy is per client in
/// reqwest, and pooled connections belong to the runtime's tokio runtime.
#[derive(Default)]
pub(crate) struct Clients {
    follow: OnceLock<reqwest::Client>,
    manual: OnceLock<reqwest::Client>,
}

impl Clients {
    fn get(&self, follow: bool) -> reqwest::Client {
        let (slot, policy) = if follow {
            (&self.follow, reqwest::redirect::Policy::limited(20))
        } else {
            (&self.manual, reqwest::redirect::Policy::none())
        };
        slot.get_or_init(|| {
            reqwest::Client::builder()
                .redirect(policy)
                .connect_timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
    }
}

struct Head {
    status: u16,
    status_text: String,
    url: String,
    headers: Vec<(String, String)>,
}

enum Event {
    Head(Head),
    Chunk(Vec<u8>),
    End,
    Error(JsError),
}

/// Node reports network failures as `TypeError: fetch failed` with the cause attached.
fn network_error(error: &reqwest::Error) -> JsError {
    let mut source: Option<&dyn std::error::Error> = Some(error);
    let mut code = None;
    let mut message = error.to_string();
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            code = Some(crate::error::io_code(io).0);
            message = io.to_string();
        }
        source = current.source();
    }
    if error.is_timeout() {
        code = Some("UND_ERR_CONNECT_TIMEOUT");
    } else if code.is_none() && error.is_connect() {
        code = Some("ECONNREFUSED");
    }
    let mut cause = serde_json::Map::new();
    cause.insert("message".into(), message.into());
    if let Some(code) = code {
        cause.insert("code".into(), code.into());
    }
    JsError::type_error("fetch failed").with_data("cause", serde_json::Value::Object(cause))
}

async fn run(client: reqwest::Client, request: FetchRequest, body: Option<Vec<u8>>, mut emit: impl FnMut(Event)) {
    let method = match reqwest::Method::from_bytes(request.method.as_bytes()) {
        Ok(method) => method,
        Err(_) => {
            return emit(Event::Error(JsError::type_error(format!("'{}' is not a valid HTTP method.", request.method))));
        }
    };
    let mut builder = client.request(method, &request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    if let Some(body) = body {
        builder = builder.body(body);
    }
    let response = match builder.send().await {
        Ok(response) => response,
        Err(error) => return emit(Event::Error(network_error(&error))),
    };
    if request.redirect == "error" && response.status().is_redirection() {
        return emit(Event::Error(JsError::type_error("fetch failed").with_data("cause", "unexpected redirect")));
    }
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_string(), String::from_utf8_lossy(value.as_bytes()).into_owned()))
        .collect();
    emit(Event::Head(Head {
        status: response.status().as_u16(),
        status_text: response.status().canonical_reason().unwrap_or("").to_string(),
        url: response.url().to_string(),
        headers,
    }));
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => emit(Event::Chunk(bytes.to_vec())),
            Err(error) => return emit(Event::Error(network_error(&error))),
        }
    }
    emit(Event::End);
}

fn release(id: u64) {
    let state = state();
    if state.resources.borrow_mut().remove(&id).is_some() {
        state.callbacks.borrow_mut().remove(&id);
        state.remove_busy();
    }
}

fn deliver(ctx: &Ctx<'_>, id: u64, event: Event) {
    let state = state();
    let Some(function) = state.callbacks.borrow().get(&id).cloned().and_then(|callback| callback.restore(ctx).ok())
    else {
        return;
    };
    let finished = matches!(event, Event::End | Event::Error(_));
    let result = (|| match event {
        Event::Head(head) => {
            let object = Object::new(ctx.clone())?;
            object.set("status", head.status)?;
            object.set("statusText", head.status_text)?;
            object.set("url", head.url)?;
            let headers = rquickjs::Array::new(ctx.clone())?;
            for (index, (name, value)) in head.headers.into_iter().enumerate() {
                headers.set(index, vec![name, value])?;
            }
            object.set("headers", headers)?;
            function.call::<_, ()>(("head", object))
        }
        Event::Chunk(bytes) => function.call::<_, ()>(("chunk", bytes_to_js(ctx, bytes)?)),
        Event::End => function.call::<_, ()>(("end",)),
        Event::Error(error) => function.call::<_, ()>(("error", error_value(ctx, &error)?)),
    })();
    if finished {
        release(id);
    }
    if let Err(error) = result {
        report_error(ctx, UncaughtKind::Exception, error);
    }
}

fn start<'js>(
    ctx: Ctx<'js>,
    request: Value<'js>,
    body: Option<String>,
    callback: Function<'js>,
) -> rquickjs::Result<u64> {
    let json = to_json(&ctx, request).map_err(|error| throw(&ctx, &error))?;
    let request: FetchRequest = serde_json::from_value(json)
        .map_err(|error| throw(&ctx, &JsError::type_error(format!("Invalid fetch request: {error}"))))?;
    let body = body.map(|text| latin1_to_bytes(&text));
    let state = state();
    let id = state.next_id();
    let sender = state.sender.clone();
    let emit = move |event: Event| {
        let task: Task = Box::new(move |ctx| deliver(ctx, id, event));
        let _ = sender.send(Message::Task(task));
    };
    let client = state.shared.http.get(request.redirect == "follow");
    let handle = state.shared.tokio.spawn(run(client, request, body, emit));
    state.resources.borrow_mut().insert(id, Box::new(handle.abort_handle()));
    state.callbacks.borrow_mut().insert(id, Persistent::save(&ctx, callback));
    state.add_busy();
    Ok(id)
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "start", |ctx: Ctx<'js>, request: Value<'js>, body: Option<String>, callback: Function<'js>| {
        start(ctx, request, body, callback)
    })?;
    set_fn(&object, "abort", |id: u64| {
        let handle = state().with_resource(id, |handle: &mut tokio::task::AbortHandle| handle.clone());
        if let Ok(handle) = handle {
            handle.abort();
            release(id);
        }
    })?;
    Ok(object)
}
