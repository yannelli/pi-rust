//! What drives a binding: the connection (holds, units, state frames, gone, link loss), the host (close) and
//! calls through its views.

use std::sync::Arc;

use super::{After, BindingState, MemberState, PendingShape, RemoteBinding, StateIn, Status};
use crate::delta::{apply_immutable, decoder};
use crate::error::{ChordError, ChordResult};
use crate::host2::connection::{ConnId, Msg, OutCall};
use crate::host2::contract::MemberDescription;
use crate::host2::errors::ChordErrorCode;
use crate::host2::exec::race_signal;
use crate::host2::object::{Ctx, clean};
use crate::host2::types::{Shape, StateEvent};
use crate::host2::wire::{RootRef, StateKind, from_wire_error};
use crate::json::Json;

impl RemoteBinding {
    /// The state members of `shape` the consumer's contract knows.
    fn state_members(&self, shape: &Shape) -> Vec<String> {
        let Some(members) = shape.variant.as_deref().and_then(|variant| self.contract.description().variant(variant))
        else {
            return Vec::new();
        };
        let is_state = |name: &&String| matches!(members.get(name.as_str()), Some(MemberDescription::State { .. }));
        shape.members.iter().filter(is_state).cloned().collect()
    }

    /// The connection sends a hold for this binding. `None`: it ended meanwhile (nothing is held). Otherwise
    /// whether the hold carries state.
    pub(crate) fn bound(&self, conn: ConnId, id: u64) -> Option<bool> {
        let changed = {
            let mut state = self.lock();
            if state.status == Status::Ended {
                return None;
            }
            state.hold = Some((conn, id));
            state.state_on = if state.views.is_empty() { state.state_on } else { state.state_views() > 0 };
            state.state_on
        };
        self.changed.notify_waiters();
        Some(changed)
    }

    /// A unit begins: `listed` are the members whose snapshots follow.
    pub(crate) fn shaped(&self, shape: Shape, listed: Vec<String>) {
        let after = {
            let mut state = self.lock();
            if state.status == Status::Ended {
                return;
            }
            let previous = state.pending.take();
            let mut missing = listed;
            // A newer shape before the older one's snapshots (relays may reorder them): wait for both.
            if let Some(previous) = &previous {
                for name in &previous.missing {
                    if !missing.contains(name) {
                        missing.push(name.clone());
                    }
                }
            }
            let wanted = self.state_members(&shape);
            missing.retain(|name| wanted.contains(name) && state.state_on);
            let values = previous.map(|previous| previous.values).unwrap_or_default();
            let done = missing.is_empty();
            state.pending = Some(PendingShape { shape, missing, values });
            if !done {
                return;
            }
            self.apply(&mut state)
        };
        self.run(after);
    }

    /// A state frame for member `name` of the current hold.
    pub(crate) fn state(&self, name: &str, frame: StateIn) {
        let mut state = self.lock();
        if state.status == Status::Ended {
            return;
        }
        if frame.kind == StateKind::Error {
            let error = frame.error.map(|error| from_wire_error(&error));
            let error = error.unwrap_or_else(|| ChordError::code_only(ChordErrorCode::Unavailable, "state error"));
            // Before the first hydration nobody could read anything: the binding fails instead of waiting forever.
            let fail = !state.hydrated && state.state_on;
            drop(state);
            if fail {
                self.gone(error);
                self.close();
            } else {
                self.report(error);
            }
            return;
        }
        if frame.kind == StateKind::Ops {
            let ops = Json::array(frame.ops.unwrap_or_default().into_iter().map(Json::array).collect());
            let in_pending = state.pending.as_ref().is_some_and(|pending| pending.values.contains_key(name));
            let target = match state.pending.as_mut().and_then(|pending| pending.values.get_mut(name)) {
                Some(target) => target,
                None => match state.members.get_mut(name) {
                    Some(target) => target,
                    None => return,
                },
            };
            let applied = target.decoder.decode_json(&ops).and_then(|ops| {
                let value = apply_immutable(Some(&target.value), &ops)?;
                target.value = value.clone();
                Ok((ops, value))
            });
            let (ops, value) = match applied {
                Ok(applied) => applied,
                Err(error) => {
                    drop(state);
                    return self.report(error);
                }
            };
            let readables = state.listeners.get(name).cloned().unwrap_or_default();
            drop(state);
            if !in_pending {
                self.run(vec![After::Deliver(readables, StateEvent::Ops { ops, value })]);
            }
            return;
        }
        let fresh = MemberState { value: frame.value.unwrap_or_default(), decoder: decoder() };
        if let Some(pending) = state.pending.as_mut()
            && (pending.missing.contains(&name.to_string()) || pending.values.contains_key(name))
        {
            pending.values.insert(name.to_string(), fresh);
            pending.missing.retain(|missing| missing != name);
            if !pending.missing.is_empty() {
                return;
            }
            let after = self.apply(&mut state);
            drop(state);
            return self.run(after);
        }
        if !self.in_variant(&state, name) {
            return;
        }
        let value = fresh.value.clone();
        state.members.insert(name.to_string(), fresh);
        let readables = state.listeners.get(name).cloned().unwrap_or_default();
        drop(state);
        self.run(vec![After::Deliver(readables, StateEvent::Reset { value })]);
    }

    /// Applies the pending shape with its snapshots: members outside the variant go, new values are announced.
    fn apply(&self, state: &mut BindingState) -> Vec<After> {
        let Some(pending) = state.pending.take() else { return Vec::new() };
        let wanted = self.state_members(&pending.shape);
        state.shape = Some(pending.shape);
        state.members.retain(|name, _| wanted.contains(name));
        let mut fresh = Vec::new();
        for (name, member) in pending.values {
            if wanted.contains(&name) {
                fresh.push((name.clone(), member.value.clone()));
                state.members.insert(name, member);
            }
        }
        state.hydrated = state.state_on && wanted.iter().all(|name| state.members.contains_key(name));
        let mut after = Vec::new();
        if state.hydrated {
            after.push(After::Settle(std::mem::take(&mut state.hydration_waiters), Ok(())));
        }
        state.status = Status::Live;
        state.applied = true;
        after.push(After::Update(state.views.clone()));
        for (name, value) in fresh {
            let readables = state.listeners.get(&name).cloned().unwrap_or_default();
            after.push(After::Deliver(readables, StateEvent::Reset { value }));
        }
        after.push(After::ShapeChanged(state.views.clone()));
        after
    }

    /// The provider ended the object (or refused the hold): every view is gone.
    pub(crate) fn gone(&self, error: ChordError) {
        let after = {
            let mut state = self.lock();
            if state.status == Status::Ended {
                return;
            }
            state.status = Status::Ended;
            state.error = Some(error.clone());
            state.hold = None;
            state.pending = None;
            state.listeners.clear();
            let waiters = std::mem::take(&mut state.hydration_waiters);
            let views = std::mem::take(&mut state.views);
            vec![After::Settle(waiters, Err(error.clone())), After::End(views, error), After::Forget]
        };
        self.run(after);
    }

    /// The link went down but may come back (parent reconnect): keep the values, wait for the re-hold.
    pub(crate) fn detached(&self) {
        let after = {
            let mut state = self.lock();
            if state.status == Status::Ended {
                return;
            }
            state.hold = None;
            state.pending = None;
            if state.status == Status::Live {
                state.status = Status::Loading;
            }
            vec![After::Update(state.views.clone())]
        };
        self.run(after);
    }

    /// No views and no linger: releases the hold and forgets the binding.
    pub(crate) fn close(&self) {
        let after = {
            let mut state = self.lock();
            let mut after = Vec::new();
            if let Some((_, timer)) = state.linger.take() {
                timer.abort();
            }
            if let Some((conn, id)) = state.hold.take() {
                after.push(After::Post(Msg::Release { conn, id }));
            }
            if state.status != Status::Ended {
                state.status = Status::Ended;
                let error = self.released();
                after.push(After::Settle(std::mem::take(&mut state.hydration_waiters), Err(error.clone())));
                after.push(After::End(std::mem::take(&mut state.views), error));
            }
            state.pending = None;
            state.listeners.clear();
            after.push(After::Forget);
            after
        };
        self.run(after);
    }

    /// Calls `name` over the current hold, waiting for it while the binding (re)holds.
    pub(crate) async fn call(self: Arc<Self>, name: String, args: Option<Json>, ctx: Ctx) -> ChordResult<Option<Json>> {
        let (conn, hold, shape) = loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.lock();
                if state.status == Status::Ended {
                    return Err(self.terminal(&state));
                }
                if let (Status::Live, Some((conn, hold))) = (state.status, state.hold) {
                    break (conn, hold, state.shape.clone());
                }
            }
            race_signal(
                async {
                    notified.await;
                    Ok(())
                },
                ctx.signal(),
            )
            .await?;
        };
        let variant = shape.and_then(|shape| shape.variant);
        let members = variant.as_deref().and_then(|variant| self.contract.description().variant(variant));
        let Some(MemberDescription::Method { args: schema, .. }) = members.and_then(|members| members.get(&name))
        else {
            let shown = variant.unwrap_or_else(|| "undefined".into());
            let message = format!("{}: {name} is not a method of variant {shown}", self.key);
            return Err(ChordError::code_only(ChordErrorCode::NotInVariant, message));
        };
        // Send only what the consumer's own schema knows (the additive rule's premise; see `ObjectImpl::call`).
        let sent = schema.as_ref().and_then(|schema| args.map(|args| clean(schema, &args)));
        let host = self.host.upgrade().ok_or_else(|| self.released())?;
        let (reply, result) = tokio::sync::oneshot::channel();
        let root = Some(RootRef { root_id: ctx.root_id().to_string(), code_version: ctx.code_version() as u64 });
        let call = OutCall { hold, member: name, args: sent, root, signal: ctx.signal().clone() };
        host.net().post(Msg::Call { conn, call, reply });
        let closed = || ChordError::code_only(ChordErrorCode::Unavailable, format!("link to {} closed", self.at));
        result.await.unwrap_or_else(|_| Err(closed()))
    }
}
