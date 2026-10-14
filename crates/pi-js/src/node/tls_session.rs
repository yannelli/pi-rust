//! Client TLS sessions for `node:tls`: the 'session' event, the `session` option, and
//! getSession. rustls session values cannot be serialized, so a session Buffer is an opaque
//! handle to tickets kept in this process.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use rustls::NamedGroup;
use rustls::client::{ClientSessionStore, ResolvesClientCert, Tls12ClientSessionValue, Tls13ClientSessionValue};
use rustls::pki_types::ServerName;

use super::net::{Event, Sender, post};
use super::tls::Verifier;

const PREFIX: &[u8] = b"pi-js tls session\0";
/// Handles kept before the oldest are forgotten, and TLS 1.3 tickets kept per group.
const MAX_HANDLES: usize = 4096;
const MAX_TICKETS: usize = 8;

/// The tickets of one chain of connections. A connection made with a session shares that
/// session's group, so a session Buffer stays usable while new tickets arrive, as in OpenSSL.
#[derive(Default)]
struct Group {
    tls13: VecDeque<Tls13ClientSessionValue>,
    tls12: Option<Tls12ClientSessionValue>,
    kx_hint: Option<NamedGroup>,
    credentials: Option<Credentials>,
}

/// rustls resumes a session only under the same verifier and client certificate resolver, so a
/// group keeps the first ones, for later connections whose settings give the same `key`.
#[derive(Clone)]
pub(crate) struct Credentials {
    pub(crate) key: u64,
    pub(crate) verifier: Arc<Verifier>,
    pub(crate) resolver: Arc<dyn ResolvesClientCert>,
}

type SharedGroup = Arc<Mutex<Group>>;

#[derive(Default)]
struct Registry {
    next: u64,
    handles: HashMap<u64, SharedGroup>,
    order: VecDeque<u64>,
}

fn registry() -> MutexGuard<'static, Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default).lock().unwrap_or_else(|poison| poison.into_inner())
}

fn register(group: &SharedGroup) -> Vec<u8> {
    let mut registry = registry();
    registry.next += 1;
    let id = registry.next;
    registry.handles.insert(id, group.clone());
    registry.order.push_back(id);
    if registry.order.len() > MAX_HANDLES
        && let Some(old) = registry.order.pop_front()
    {
        registry.handles.remove(&old);
    }
    [PREFIX, &id.to_be_bytes()].concat()
}

fn lookup(handle: &[u8]) -> Option<SharedGroup> {
    let id = handle.strip_prefix(PREFIX)?.try_into().ok().map(u64::from_be_bytes)?;
    registry().handles.get(&id).cloned()
}

/// The session store of one client connection, linked to its socket when the handshake starts.
pub(crate) struct SessionStore {
    group: SharedGroup,
    link: OnceLock<(Sender, u64)>,
}

impl fmt::Debug for SessionStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionStore")
    }
}

impl SessionStore {
    /// A store that resumes from `session` (a handle from an earlier connection) when it is known.
    pub(crate) fn new(session: Option<&[u8]>) -> Arc<Self> {
        let group = session.and_then(lookup).unwrap_or_default();
        Arc::new(Self { group, link: OnceLock::new() })
    }

    pub(crate) fn link(&self, sender: &Sender, id: u64) {
        let _ = self.link.set((sender.clone(), id));
    }

    /// The group's credentials when they were made with the same settings.
    pub(crate) fn credentials(&self, key: u64) -> Option<Credentials> {
        self.group().credentials.clone().filter(|credentials| credentials.key == key)
    }

    pub(crate) fn keep_credentials(&self, credentials: Credentials) {
        self.group().credentials.get_or_insert(credentials);
    }

    /// The handle `getSession()` returns. It resumes once the server has sent a ticket.
    pub(crate) fn handle(&self) -> Vec<u8> {
        register(&self.group)
    }

    fn group(&self) -> MutexGuard<'_, Group> {
        self.group.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn announce(&self) {
        if let Some((sender, id)) = self.link.get() {
            post(sender, *id, Event::Session(self.handle()));
        }
    }
}

impl ClientSessionStore for SessionStore {
    fn set_kx_hint(&self, _: ServerName<'static>, group: NamedGroup) {
        self.group().kx_hint = Some(group);
    }

    fn kx_hint(&self, _: &ServerName<'_>) -> Option<NamedGroup> {
        self.group().kx_hint
    }

    fn set_tls12_session(&self, _: ServerName<'static>, value: Tls12ClientSessionValue) {
        self.group().tls12 = Some(value);
        self.announce();
    }

    fn tls12_session(&self, _: &ServerName<'_>) -> Option<Tls12ClientSessionValue> {
        self.group().tls12.clone()
    }

    fn remove_tls12_session(&self, _: &ServerName<'static>) {
        self.group().tls12 = None;
    }

    fn insert_tls13_ticket(&self, _: ServerName<'static>, value: Tls13ClientSessionValue) {
        let mut group = self.group();
        if group.tls13.len() == MAX_TICKETS {
            group.tls13.pop_front();
        }
        group.tls13.push_back(value);
        drop(group);
        self.announce();
    }

    fn take_tls13_ticket(&self, _: &ServerName<'static>) -> Option<Tls13ClientSessionValue> {
        self.group().tls13.pop_back()
    }
}
