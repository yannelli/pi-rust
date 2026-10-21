//! Addresses and acquiring references: where an object of a type lives and the one object per address.

use std::sync::Arc;

use super::HostImpl;
use crate::error::{ChordError, ChordResult};
use crate::host2::contract::{Contract, Scope, bind, resolve_host};
use crate::host2::errors::{ChordErrorCode, invalid};
use crate::host2::object::{
    HostCore, ObjectIdentity, ObjectImpl, ObjectStatus, Ref, RefOwner, Service, await_ready, reaches,
};
use crate::host2::schema::{canonical_identity, normalize};
use crate::host2::types::GetOptions;
use crate::json::Json;
use crate::sync::BoxFuture;

/// An object's address, normalized input and the identity it is for.
struct Located {
    address: String,
    input: Json,
    principal: Json,
}

fn unknown_type(type_name: &str, host: &str) -> ChordError {
    let message = format!("{type_name} is not served by {host}");
    ChordError::with_reason(ChordErrorCode::Unavailable, "unknown_type", message)
}

impl HostImpl {
    pub(super) fn address(
        &self,
        contract: &Contract,
        input: &Json,
        principal: &Json,
        candidate: bool,
    ) -> ChordResult<String> {
        if let Some((_, normalized)) = self.elsewhere(contract, input)? {
            return Ok(self.remote_key(contract, &normalized, principal));
        }
        let service = self.binding_service(contract.name(), candidate);
        let service = service.ok_or_else(|| unknown_type(contract.name(), self.name()))?;
        Ok(self.locate(&service, input, principal)?.address)
    }

    /// Where an object of a remote type lives when that is another host: its name and the input normalized
    /// with the caller's contract. `None`: here (and every local type).
    fn elsewhere(&self, contract: &Contract, input: &Json) -> ChordResult<Option<(String, Json)>> {
        let placement = match contract.description().host() {
            Some(placement) if contract.scope() == Scope::Remote => placement,
            _ => return Ok(None),
        };
        let normalized = normalize(contract.input(), Some(input), &format!("{} input", contract.name()))?;
        let at = resolve_host(contract.name(), placement, &normalized)?;
        Ok((at != self.name()).then_some((at, normalized)))
    }

    /// The address of `input` for an identity, normalized. A `lifetime: "host"` type has one object: the
    /// empty input, for the host's own identity, whoever asks; another input is invalid.
    fn locate(&self, service: &Service, input: &Json, principal: &Json) -> ChordResult<Located> {
        let (name, schema) = (service.contract.name(), service.contract.input());
        let what = format!("{name} input");
        let normalized = normalize(schema, Some(input), &what)?;
        let identity = canonical_identity(schema, &normalized);
        let mut principal = principal.clone();
        if service.host_lifetime() {
            let empty = normalize(schema, Some(&Json::empty_object()), &what)?;
            if identity != canonical_identity(schema, &empty) {
                let message = format!("{name} has lifetime \"host\": its one object has the empty input");
                return Err(invalid("input", message, None));
            }
            principal = self.principal().clone();
        }
        let address = format!("{name}\u{0}{identity}\u{0}{}", self.principal_part(&principal));
        Ok(Located { address, input: normalized, principal })
    }

    /// The key of a remote binding: type, input identity, identity key and the consumer's fingerprint.
    fn remote_key(&self, contract: &Contract, normalized: &Json, principal: &Json) -> String {
        let identity = canonical_identity(contract.input(), normalized);
        let part = self.principal_part(principal);
        format!("@{}\u{0}{identity}\u{0}{part}\u{0}{}", contract.name(), contract.fingerprint())
    }

    /// The service a reference binds against: the current one, or for a reload's rerun (`candidate`) the
    /// running reload's pending one.
    fn binding_service(&self, type_name: &str, candidate: bool) -> Option<Arc<Service>> {
        let pending = candidate
            .then(|| self.state().pending.as_ref().and_then(|pending| pending.services.get(type_name).cloned()))
            .flatten();
        pending.or_else(|| self.service(type_name))
    }

    /// A reference on an object of another host. Remote references arrive with links (phase 3).
    fn acquire_remote(&self, contract: &Contract, at: &str) -> ChordResult<Ref> {
        let message = format!("{} lives on {at}, and {} has no link to it", contract.name(), self.name());
        Err(ChordError::with_reason(ChordErrorCode::Unavailable, "no_link", message))
    }

    pub(super) fn acquire_ref(
        &self,
        contract: &Contract,
        input: &Json,
        options: &GetOptions,
        owner: RefOwner,
        from: Option<&Arc<ObjectImpl>>,
        candidate: bool,
    ) -> ChordResult<Ref> {
        if let Some((at, _)) = self.elsewhere(contract, input)? {
            return self.acquire_remote(contract, &at);
        }
        let service = self.binding_service(contract.name(), candidate);
        let service = service.ok_or_else(|| unknown_type(contract.name(), self.name()))?;
        bind(&service.contract, service.options.accepts.as_ref(), contract)?;
        let principal = options.principal.clone().unwrap_or_else(|| self.principal().clone());
        let located = self.locate(&service, input, &principal)?;
        // The provider's own placement decides, never where a request says it is going.
        if let Some(placement) = service.contract.description().host() {
            let at = resolve_host(contract.name(), placement, &located.input)?;
            if at != self.name() {
                let message = format!("{} with this input lives on {at}, not {}", contract.name(), self.name());
                return Err(invalid("placement", message, None));
            }
        }
        loop {
            let found = self.state().objects.get(&located.address).cloned();
            if let Some(object) = found {
                if let Some(from) = from
                    && (Arc::ptr_eq(&object, from) || reaches(&object, from))
                {
                    let message = format!("{} -> {} would create a ref cycle", from.address(), located.address);
                    return Err(invalid("cycle", message, None));
                }
                match object.new_ref(contract.clone(), owner) {
                    Some(reference) => return Ok(Ref::new(reference)),
                    // Failed or disposing, not forgotten yet: forget it here and create the next object.
                    None => self.forget_object(&object),
                }
                continue;
            }
            if self.state().gating.as_ref().is_some_and(|gating| gating.contains(contract.name())) {
                let message = format!("{} is being upgraded; retry", contract.name());
                return Err(ChordError::with_reason(ChordErrorCode::UpgradeConflict, "gate", message));
            }
            let dying = self.state().dying.get(&located.address).cloned();
            let after = match dying {
                Some(dying) if dying.status() == ObjectStatus::Disposed => {
                    self.disposed(&dying);
                    None
                }
                // A new object for an address still being disposed waits for that cleanup (exclusive resources).
                Some(dying) if dying.disposing() => Some(dying.dispose(false, None)),
                _ => None,
            };
            let identity = ObjectIdentity {
                address: located.address.clone(),
                type_name: contract.name().to_string(),
                input: located.input.clone(),
                principal: located.principal.clone(),
                host_lifetime: service.host_lifetime(),
            };
            let object = ObjectImpl::new(self.core(), identity, after);
            {
                let mut state = self.state();
                if state.objects.contains_key(&located.address) {
                    continue;
                }
                state.objects.insert(located.address.clone(), object.clone());
            }
            let reference = object.new_ref(contract.clone(), owner);
            object.launch();
            if let Some(reference) = reference {
                return Ok(Ref::new(reference));
            }
        }
    }

    pub(super) fn forget_object(&self, object: &ObjectImpl) {
        let disposing = object.disposing();
        let mut state = self.state();
        if state.objects.get(object.address()).is_some_and(|known| std::ptr::eq(Arc::as_ptr(known), object)) {
            state.objects.shift_remove(object.address());
        }
        if disposing && let Some(object) = object.me() {
            state.dying.insert(object.address().to_string(), object);
        }
    }

    pub(super) fn get(&self, contract: &Contract, input: Json, options: GetOptions) -> BoxFuture<ChordResult<Ref>> {
        let reference = match self.acquire_ref(contract, &input, &options, RefOwner::External, None, false) {
            Ok(reference) => reference,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        let signals: Vec<_> = options.signal.into_iter().collect();
        let timeout = self.ready_timeout();
        let guard = ReleaseOnDrop(Some(reference.clone()));
        Box::pin(async move {
            await_ready(&reference, &signals, timeout).await?;
            guard.disarm();
            Ok(reference)
        })
    }
}

/// Releases the reference of a `get` whose future is dropped before it settles.
struct ReleaseOnDrop(Option<Ref>);

impl ReleaseOnDrop {
    fn disarm(mut self) {
        self.0.take();
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        if let Some(reference) = self.0.take() {
            reference.release();
        }
    }
}
