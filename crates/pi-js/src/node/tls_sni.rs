//! SNICallback and addContext for TLS servers: the handshake stops after the ClientHello while
//! JavaScript picks a secure context for the server name, then goes on with that context.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::function::Opt;
use rquickjs::{Ctx, Object, Value};
use rustls::ServerConfig;
use tokio::sync::oneshot;
use tokio_rustls::LazyConfigAcceptor;
use tokio_rustls::server::TlsStream;

use super::net::{BoxIo, Sender};
use super::set_fn;
use super::tls::{Tls, handshake_error};
use crate::error::JsError;
use crate::runtime::convert::{caught, error_from_value, throw};
use crate::runtime::state::{Message, Task, state};
use crate::runtime::{UncaughtKind, report_error};

type Choice = Result<Option<Arc<ServerConfig>>, JsError>;

/// A handshake waiting for JavaScript's answer.
struct Pending(oneshot::Sender<Choice>);

/// Whether a server asks JavaScript for a context; addContext can turn it on after listen.
pub(crate) struct SniFlag(pub(crate) Arc<AtomicBool>);

/// Node's error when the server drops the connection during the handshake.
fn hang_up() -> JsError {
    JsError::new("socket hang up").with_code("ECONNRESET")
}

/// Post the server name to the listener's callback and wait for the context it picks.
async fn ask(sender: &Sender, listener: u64, name: String) -> Choice {
    let (reply, answer) = oneshot::channel();
    let task: Task = Box::new(move |ctx| {
        let state = state();
        let request = state.insert_resource(Box::new(Pending(reply)));
        let callback = state.callbacks.borrow().get(&listener).cloned();
        let Some(function) = callback.and_then(|callback| callback.restore(ctx).ok()) else {
            state.resources.borrow_mut().remove(&request);
            return;
        };
        if let Err(error) = function.call::<_, ()>(("sni", name, request)) {
            report_error(ctx, UncaughtKind::Exception, error);
        }
    });
    if sender.send(Message::Task(task)).is_err() {
        return Err(hang_up());
    }
    answer.await.unwrap_or_else(|_| Err(hang_up()))
}

/// Accept with the listener's default config unless JavaScript picks another for the SNI name.
pub(crate) async fn accept(
    default: Arc<ServerConfig>,
    io: BoxIo,
    sender: &Sender,
    listener: u64,
) -> Result<TlsStream<BoxIo>, JsError> {
    let acceptor = LazyConfigAcceptor::new(rustls::server::Acceptor::default(), io);
    let start = acceptor.await.map_err(|error| handshake_error(&error, None))?;
    let config = match start.client_hello().server_name().map(str::to_owned) {
        Some(name) => ask(sender, listener, name).await?.unwrap_or(default),
        None => default,
    };
    start.into_stream(config).await.map_err(|error| handshake_error(&error, None))
}

/// Answer a pending handshake: an error fails it, options build its config, and `undefined`
/// keeps the default context.
fn answer<'js>(ctx: Ctx<'js>, request: u64, error: Value<'js>, options: Opt<Value<'js>>) -> rquickjs::Result<()> {
    let Some(resource) = state().resources.borrow_mut().remove(&request) else { return Ok(()) };
    let Ok(pending) = resource.downcast::<Pending>() else { return Ok(()) };
    let choice = if !error.is_undefined() && !error.is_null() {
        Err(error_from_value(&ctx, error))
    } else {
        match options.0.filter(|value| !value.is_undefined() && !value.is_null()) {
            Some(options) => match Tls::from_js(&ctx, options, true) {
                Ok(tls) => Ok(tls.and_then(|tls| tls.server_config())),
                Err(error) => Err(caught(&ctx, error)),
            },
            None => Ok(None),
        }
    };
    let _ = pending.0.send(choice);
    Ok(())
}

pub(super) fn natives<'js>(object: &Object<'js>) -> rquickjs::Result<()> {
    set_fn(object, "sniDone", answer)?;
    set_fn(object, "sniFlag", |enabled: bool| state().insert_resource(Box::new(SniFlag(Arc::new(enabled.into())))))?;
    set_fn(object, "setSni", |ctx: Ctx<'js>, flag: u64, enabled: bool| {
        let result = state().with_resource(flag, |flag: &mut SniFlag| flag.0.store(enabled, Ordering::SeqCst));
        result.map_err(|error| throw(&ctx, &error))
    })?;
    Ok(())
}
