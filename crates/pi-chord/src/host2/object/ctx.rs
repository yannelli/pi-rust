//! Call contexts ([`Ctx`]): the root a call belongs to, its code version, identity and abort signal.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{HostCore, ObjectImpl, Ref, RefOwner, RootFn};
use crate::context::{AbortController, AbortSignal};
use crate::error::{ChordError, ChordResult};
use crate::host2::contract::Contract;
use crate::host2::errors::{ChordErrorCode, ErrorOptions, invalid};
use crate::host2::exec::{Linked, race_signal};
use crate::host2::types::GetOptions;
use crate::json::Json;
use crate::sync::{BoxFuture, lock};

/// Options of a root: who it is for (default: the host's own identity), when it aborts, and the code
/// version it runs in (default: the current one; links carry code versions).
#[derive(Clone, Default)]
pub struct RootOptions {
    pub principal: Option<Json>,
    pub signal: Option<AbortSignal>,
    pub code_version: Option<i64>,
}

/// The fields of a new context. `signals`: it aborts with the first of them and stops following them at close.
pub(crate) struct CtxFields {
    pub signals: Vec<AbortSignal>,
    pub root_id: String,
    pub code_version: i64,
    pub principal: Json,
    /// Code holders (tasks): every call they make starts a fresh root.
    pub holder: bool,
}

struct CtxInner {
    host: Arc<dyn HostCore>,
    linked: Linked,
    signal: AbortSignal,
    root_id: String,
    code_version: i64,
    principal: Json,
    holder: bool,
    refs: Mutex<Vec<Ref>>,
    /// Aborted when the context closes: awaited `get`s give up.
    closing: AbortController,
    closed: AtomicBool,
}

/// What a call, listener, task or root receives: cheap to clone, all clones are the same context.
#[derive(Clone)]
pub struct Ctx(Arc<CtxInner>);

fn expired() -> ChordError {
    let options = ErrorOptions::reason("expired");
    ChordError::coded(ChordErrorCode::Aborted, "ctx expired: its call already settled", options)
}

impl Ctx {
    pub(crate) fn new(host: Arc<dyn HostCore>, fields: CtxFields) -> Self {
        let linked = Linked::new(&fields.signals);
        Self(Arc::new(CtxInner {
            host,
            signal: linked.signal(),
            linked,
            root_id: fields.root_id,
            code_version: fields.code_version,
            principal: fields.principal,
            holder: fields.holder,
            refs: Mutex::default(),
            closing: AbortController::new(),
            closed: AtomicBool::new(false),
        }))
    }

    pub fn signal(&self) -> &AbortSignal {
        &self.0.signal
    }

    pub fn root_id(&self) -> &str {
        &self.0.root_id
    }

    pub fn code_version(&self) -> i64 {
        self.0.code_version
    }

    /// Who the root is for.
    pub fn principal(&self) -> &Json {
        &self.0.principal
    }

    pub fn holder(&self) -> bool {
        self.0.holder
    }

    /// True after the call or root that received this context settled: it may no longer start work.
    pub fn closed(&self) -> bool {
        self.0.closed.load(Ordering::SeqCst)
    }

    pub fn ptr_eq(&self, other: &Ctx) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Fails unless this context may start work on host `host` now: not expired, aborted or foreign.
    pub fn check(&self, host: u64) -> ChordResult<()> {
        if self.0.host.id() != host {
            return Err(invalid("foreign_ctx", "ctx belongs to another host", None));
        }
        if self.closed() {
            return Err(expired());
        }
        if let Some(reason) = self.0.signal.reason() {
            // A forced retirement aborted the root: its code version is gone, so the call is `superseded`.
            if reason.code() == Some(ChordErrorCode::Superseded) {
                let message = reason.to_string();
                let options = ErrorOptions::default().with_cause(reason);
                return Err(ChordError::coded(ChordErrorCode::Superseded, message, options));
            }
            let options = ErrorOptions::default().with_cause(reason);
            return Err(ChordError::coded(ChordErrorCode::Aborted, "ctx aborted", options));
        }
        Ok(())
    }

    /// A call-owned reference, released when this context closes; resolves once the object is ready.
    pub fn get(&self, contract: &Contract, input: Json, options: GetOptions) -> BoxFuture<ChordResult<Ref>> {
        let acquired = (|| {
            self.check(self.0.host.id())?;
            let principal = Some(options.principal.unwrap_or_else(|| self.0.principal.clone()));
            let options = GetOptions { principal, ..options };
            self.0.host.acquire(contract, &input, &options, RefOwner::Call, None, false)
        })();
        let reference = match acquired {
            Ok(reference) => reference,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        lock(&self.0.refs).push(reference.clone());
        let signals = [self.0.signal.clone(), self.0.closing.signal()];
        let timeout = self.0.host.ready_timeout();
        Box::pin(async move {
            await_ready(&reference, &signals, timeout).await?;
            Ok(reference)
        })
    }

    /// A context for a call into `object`: same root and code version, its own call-owned references, aborted
    /// on forced disposal. Code inside an object runs for the host's own identity.
    pub(crate) fn child(&self, object: &ObjectImpl) -> Ctx {
        Ctx::new(
            self.0.host.clone(),
            CtxFields {
                signals: vec![self.0.signal.clone(), object.call_abort.signal()],
                root_id: self.0.root_id.clone(),
                code_version: self.0.code_version,
                principal: self.0.host.principal().clone(),
                holder: false,
            },
        )
    }

    pub(crate) fn close(&self) {
        self.0.closed.store(true, Ordering::SeqCst);
        self.0.closing.abort(Some(expired()));
        self.0.linked.release();
        let refs = std::mem::take(&mut *lock(&self.0.refs));
        for reference in refs {
            reference.release();
        }
    }
}

impl std::fmt::Debug for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx").field("root_id", &self.0.root_id).field("closed", &self.closed()).finish()
    }
}

/// Runs `run` in a new root of `host`. The root's id is taken now; `run` starts when the future is polled.
pub(crate) fn run_root<T, F, Fut>(host: &Arc<dyn HostCore>, options: RootOptions, run: F) -> BoxFuture<ChordResult<T>>
where
    T: Send + 'static,
    F: FnOnce(Ctx) -> Fut + Send + 'static,
    Fut: Future<Output = ChordResult<T>> + Send + 'static,
{
    let slot: Arc<Mutex<Option<T>>> = Arc::default();
    let out = slot.clone();
    let erased: RootFn = Box::new(move |ctx| {
        Box::pin(async move {
            let value = run(ctx).await?;
            *lock(&out) = Some(value);
            Ok(())
        })
    });
    let root = host.root_erased(erased, options);
    Box::pin(async move {
        root.await?;
        let value = lock(&slot).take();
        value.ok_or_else(|| ChordError::error("root settled without a value"))
    })
}

/// Waits until `reference` is ready, at most `timeout` and until a signal aborts. On failure the reference
/// is released and the error returned: the activation error, `aborted`, or `unavailable/ready_timeout`.
pub(crate) async fn await_ready(
    reference: &Ref,
    signals: &[AbortSignal],
    timeout: Option<Duration>,
) -> ChordResult<()> {
    let linked = Linked::new(signals);
    let signal = linked.signal();
    let ready = reference.ready();
    let result = match timeout {
        Some(timeout) => {
            let timed = async {
                match tokio::time::timeout(timeout, ready).await {
                    Ok(result) => result,
                    Err(_) => {
                        let message = format!("{} was not ready after {} ms", reference.address(), timeout.as_millis());
                        Err(ChordError::with_reason(ChordErrorCode::Unavailable, "ready_timeout", message))
                    }
                }
            };
            race_signal(timed, &signal).await
        }
        None => race_signal(ready, &signal).await,
    };
    linked.release();
    if result.is_err() {
        reference.release();
    }
    result
}
