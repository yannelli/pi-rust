//! Calls into an object: admission, leases, the write check of its cells, and invocations (methods and
//! listener events) that keep the object alive until they settle.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;

use super::{Ctx, Member, ObjectImpl, ObjectStatus, RefCore, RefImpl};
use crate::error::{ChordError, ChordResult};
use crate::host2::cell::WriteCheck;
use crate::host2::contract::{LOCAL_VARIANT, MemberDescription, Scope};
use crate::host2::errors::{ChordErrorCode, invalid};
use crate::host2::exec::{in_factory, panic_error, race_signal};
use crate::host2::schema::assert_valid;
use crate::host2::types::RefStatus;
use crate::json::Json;
use crate::sync::BoxFuture;

/// Counts one running invocation; the last one to settle lets the object go idle.
struct Invocation(Arc<ObjectImpl>);

impl Drop for Invocation {
    fn drop(&mut self) {
        let idle = {
            let mut state = self.0.state();
            state.invocations -= 1;
            state.invocations == 0
        };
        if idle {
            self.0.idle.notify_waiters();
            self.0.maybe_idle();
        }
    }
}

/// `pi_schema::clean` on a JSON value: drops what the schema does not know.
fn clean(schema: &Json, value: &Json) -> Json {
    Json::from(pi_schema::clean(&schema.to_serde(), value.to_serde()))
}

impl ObjectImpl {
    /// Admission for a call or write carrying `ctx`.
    pub(crate) fn admit(&self, ctx: &Ctx) -> ChordResult<()> {
        let host = &self.host;
        if host.failed().is_some() {
            let message = format!("{} failed and admits no work", host.name());
            return Err(ChordError::with_reason(ChordErrorCode::Unavailable, "host_failed", message));
        }
        let code_version = ctx.code_version();
        if !host.has_code_version(code_version) {
            let message = format!("code version {code_version} is retired");
            return Err(ChordError::code_only(ChordErrorCode::Superseded, message));
        }
        let state = self.state();
        if code_version < state.min_code_version {
            let message = format!("{} was upgraded in code version {}", self.address(), state.min_code_version);
            return Err(ChordError::code_only(ChordErrorCode::UpgradeConflict, message));
        }
        if let Some(gate) = &state.gate
            && !gate.grandfathered.contains(ctx.root_id())
        {
            let message = format!("{} is being upgraded", self.address());
            return Err(ChordError::code_only(ChordErrorCode::UpgradeConflict, message));
        }
        Ok(())
    }

    /// Records that the root of `ctx` touched this object (a reload gate grandfathers it).
    pub(crate) fn lease(self: &Arc<Self>, ctx: &Ctx) {
        {
            let mut state = self.state();
            if state.leases.contains_key(ctx.root_id()) {
                return;
            }
            state.leases.insert(ctx.root_id().to_string(), ctx.code_version());
        }
        self.host.touch(ctx.root_id(), self);
    }

    /// A root that touched this object ended: its lease ends, and a gate waiting for it may drain.
    pub(crate) fn root_ended(&self, root_id: &str) {
        let drained = {
            let mut state = self.state();
            state.leases.remove(root_id);
            match state.gate.as_mut() {
                Some(gate) if gate.grandfathered.contains(root_id) => {
                    gate.grandfathered.remove(root_id);
                    if gate.grandfathered.is_empty() { gate.drained.take() } else { None }
                }
                _ => None,
            }
        };
        if let Some(drained) = drained {
            drained();
        }
    }

    /// What the object's cells check before a write.
    pub(crate) fn write_check(&self) -> WriteCheck {
        let me = self.me.clone();
        Arc::new(move |ctx: &Ctx| match me.upgrade() {
            Some(object) => object.guard(ctx),
            None => Err(ChordError::code_only(ChordErrorCode::Gone, "object is disposed")),
        })
    }

    fn guard(&self, ctx: &Ctx) -> ChordResult<()> {
        if in_factory(self.host.id()) {
            return Err(invalid("read_only", "cells cannot be written while a factory runs", None));
        }
        ctx.check(self.host.id())?;
        if ctx.signal().aborted() {
            return Err(ChordError::code_only(ChordErrorCode::Aborted, "write with an aborted ctx"));
        }
        if self.status() == ObjectStatus::Disposed {
            return Err(ChordError::code_only(ChordErrorCode::Gone, format!("{} is disposed", self.address())));
        }
        if ctx.root_id().starts_with("task:") {
            return Ok(());
        }
        self.admit(ctx)
    }

    /// Calls method `name` through `reference`. `None`: the method is void.
    pub(crate) fn call(
        self: &Arc<Self>,
        reference: Arc<RefImpl>,
        name: String,
        args: Option<Json>,
        ctx: Ctx,
    ) -> BoxFuture<ChordResult<Option<Json>>> {
        let object = self.clone();
        Box::pin(async move { object.call_inner(&reference, &name, args, &ctx).await })
    }

    async fn call_inner(
        self: &Arc<Self>,
        reference: &RefImpl,
        name: &str,
        args: Option<Json>,
        ctx: &Ctx,
    ) -> ChordResult<Option<Json>> {
        ctx.check(self.host.id())?;
        if reference.status().ended() {
            return Err(reference.terminal_error());
        }
        if reference.status() == RefStatus::Loading {
            race_signal(reference.ready_task(), ctx.signal()).await?;
        }
        let status = self.status();
        if reference.status() != RefStatus::Live || status != ObjectStatus::Live {
            return Err(match reference.status() {
                RefStatus::Live => {
                    ChordError::code_only(ChordErrorCode::Gone, format!("{} is {}", self.address(), status.as_str()))
                }
                _ => reference.terminal_error(),
            });
        }
        self.admit(ctx)?;
        let code_version = ctx.code_version();
        let Some(installed) = self.installed(code_version) else {
            let message = format!("{} has no implementation for code version {code_version}", self.address());
            return Err(ChordError::code_only(ChordErrorCode::Superseded, message));
        };
        let view = reference.view_of(&installed)?;
        let variant = view.variant_name();
        let remote = reference.contract().scope() == Scope::Remote;
        let wanted = reference.describe(if remote { variant } else { Some(LOCAL_VARIANT) }, name);
        let method = match view.get(name) {
            Some(Member::Method(method)) if !remote || matches!(wanted, Some(MemberDescription::Method { .. })) => {
                method.clone()
            }
            _ => {
                let shown = variant.unwrap_or("(local)");
                let message = format!("{}: {name} is not a method of variant {shown}", self.address());
                return Err(ChordError::code_only(ChordErrorCode::NotInVariant, message));
            }
        };
        let schema = if Arc::ptr_eq(&view, &installed.implementation) {
            let members = installed.service.contract.description().variant(variant.unwrap_or(LOCAL_VARIANT));
            members.and_then(|members| members.get(name)).cloned()
        } else {
            wanted.clone()
        };
        let (described, args_schema, result_schema) = match schema {
            Some(MemberDescription::Method { args, result }) => (true, args, result),
            _ => (false, None, None),
        };
        // Remote calls are validated; no arguments: the handler gets `null`; void: the caller gets `None`.
        let (validated_args, validated_result) =
            if remote { (args_schema.clone(), result_schema.clone()) } else { (None, None) };
        let takes_args = !described || args_schema.is_some();
        let returns = !described || result_schema.is_some();
        let mut input = None;
        if takes_args {
            // Strip with the consumer's schema, then with the provider's, before validating (the additive rule).
            let own = match (&wanted, remote) {
                (Some(MemberDescription::Method { args: Some(own), .. }), true) => Some(own.clone()),
                _ => None,
            };
            let sent = match (own, args) {
                (Some(own), Some(args)) => Some(clean(&own, &args)),
                (_, args) => args,
            };
            input = match (&validated_args, sent) {
                (Some(schema), Some(sent)) => Some(clean(schema, &sent)),
                (_, sent) => sent,
            };
            if let Some(schema) = &validated_args {
                assert_valid("args", &format!("{}.{name} args", self.address()), schema, input.as_ref())?;
            }
        }
        self.lease(ctx);
        let child = ctx.child(self);
        let result = self.invoke(method(input.unwrap_or_default(), child.clone())).await;
        child.close();
        let result = result?;
        if !returns {
            return Ok(None);
        }
        if let Some(schema) = &validated_result {
            assert_valid("result", &format!("{}.{name} result", self.address()), schema, Some(&result))?;
        }
        Ok(Some(result))
    }

    /// Runs an invocation (method or listener) that keeps the object alive until it settles.
    pub(crate) async fn invoke<T>(self: &Arc<Self>, work: impl Future<Output = ChordResult<T>>) -> ChordResult<T> {
        self.state().invocations += 1;
        let invocation = Invocation(self.clone());
        let result = AssertUnwindSafe(work).catch_unwind().await;
        drop(invocation);
        result.unwrap_or_else(|panic| Err(panic_error(panic.as_ref())))
    }

    /// Settles once no invocation runs.
    pub(crate) async fn wait_invocations(&self) {
        loop {
            let notified = self.idle.notified();
            if self.state().invocations == 0 {
                return;
            }
            notified.await;
        }
    }
}
