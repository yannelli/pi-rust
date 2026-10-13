//! A deterministic in-memory server host whose Sessions record every routed call.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use pi_chord::services::{ServiceCall, ServiceUpdatePublisher};
use pi_chord::{BoxFuture, ChordError, ChordResult, Context, Json, json};
use tokio::sync::watch;

use crate::errors::ServerError;
use crate::task::lock;
use crate::types::{
    BasicSessionMetadata, RoutedServerPresentation, RoutedServerServiceAttachment, RoutedServerServiceHost,
    RoutedSessionAttachment, RoutedSessionHandle, ServerHost,
};

/// A value resolved once and awaited by any number of tasks.
pub struct Deferred<T> {
    sender: Arc<watch::Sender<Option<T>>>,
}

impl<T> Clone for Deferred<T> {
    fn clone(&self) -> Self {
        Self { sender: self.sender.clone() }
    }
}

impl<T: Clone + Send + Sync> Default for Deferred<T> {
    fn default() -> Self {
        Self { sender: Arc::new(watch::channel(None).0) }
    }
}

impl<T: Clone + Send + Sync> Deferred<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve with `value`. Later calls have no effect.
    pub fn resolve(&self, value: T) {
        self.sender.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(value);
            true
        });
    }

    pub async fn wait(&self) -> T {
        let mut receiver = self.sender.subscribe();
        let value = receiver.wait_for(Option::is_some).await;
        value.ok().and_then(|value| value.clone()).unwrap_or_else(|| unreachable!("the sender outlives waiters"))
    }

    pub fn is_resolved(&self) -> bool {
        self.sender.borrow().is_some()
    }
}

/// Holds one operation until released, and reports when it has started.
#[derive(Clone, Default)]
pub struct Gate {
    pub entered: Deferred<()>,
    pub release: Deferred<()>,
}

#[derive(Default)]
struct HarnessState {
    attached_clients: i64,
    attachment_release_count: usize,
    close_count: usize,
    service_calls: Vec<ServiceCall>,
    fail_attachment_release: Option<ChordError>,
    fail_close: Option<ChordError>,
    next_service_error: Option<ChordError>,
    next_service_result: Option<Option<Json>>,
    close_gate: Option<Gate>,
    service_gate: Option<Gate>,
}

/// One hosted Session. Calls return `{ ok: true }` unless configured otherwise.
pub struct TestHarness {
    pub metadata: BasicSessionMetadata,
    pub closed: Deferred<()>,
    termination: Deferred<Option<ChordError>>,
    state: Mutex<HarnessState>,
    me: Weak<TestHarness>,
}

impl TestHarness {
    pub fn new(metadata: BasicSessionMetadata) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            metadata,
            closed: Deferred::new(),
            termination: Deferred::new(),
            state: Mutex::new(HarnessState::default()),
            me: me.clone(),
        })
    }

    pub fn attached_clients(&self) -> i64 {
        lock(&self.state).attached_clients
    }

    pub fn attachment_release_count(&self) -> usize {
        lock(&self.state).attachment_release_count
    }

    pub fn close_count(&self) -> usize {
        lock(&self.state).close_count
    }

    pub fn service_calls(&self) -> Vec<ServiceCall> {
        lock(&self.state).service_calls.clone()
    }

    pub fn set_fail_attachment_release(&self, error: Option<ChordError>) {
        lock(&self.state).fail_attachment_release = error;
    }

    pub fn set_fail_close(&self, error: Option<ChordError>) {
        lock(&self.state).fail_close = error;
    }

    pub fn set_next_service_error(&self, error: ChordError) {
        lock(&self.state).next_service_error = Some(error);
    }

    /// The next call's result; `None` means a void result.
    pub fn set_next_service_result(&self, result: Option<Json>) {
        lock(&self.state).next_service_result = Some(result);
    }

    pub fn gate_next_close(&self) -> Gate {
        let gate = Gate::default();
        lock(&self.state).close_gate = Some(gate.clone());
        gate
    }

    pub fn gate_next_service_call(&self) -> Gate {
        let gate = Gate::default();
        lock(&self.state).service_gate = Some(gate.clone());
        gate
    }

    /// Report an unexpected end of the Session.
    pub fn terminate(&self, error: ChordError) {
        self.termination.resolve(Some(error));
    }

    pub async fn wait_terminated(&self) -> Option<ChordError> {
        self.termination.wait().await
    }

    pub async fn invoke(&self, call: ServiceCall) -> ChordResult<Option<Json>> {
        let gate = {
            let mut state = lock(&self.state);
            state.service_calls.push(call);
            if let Some(error) = state.next_service_error.take() {
                return Err(error);
            }
            state.service_gate.take()
        };
        if let Some(gate) = gate {
            gate.entered.resolve(());
            gate.release.wait().await;
        }
        Ok(lock(&self.state).next_service_result.take().unwrap_or_else(|| Some(json!({ "ok": true }))))
    }
}

struct HarnessAttachment {
    harness: Arc<TestHarness>,
    released: Mutex<bool>,
}

#[async_trait]
impl RoutedSessionAttachment for HarnessAttachment {
    async fn invoke_service(
        &self,
        call: ServiceCall,
        _: ServiceUpdatePublisher,
        _: &Context,
    ) -> ChordResult<Option<Json>> {
        self.harness.invoke(call).await
    }

    async fn release(&self, _context: &Context) -> ChordResult<()> {
        let mut released = lock(&self.released);
        if *released {
            return Ok(());
        }
        let mut state = lock(&self.harness.state);
        state.attachment_release_count += 1;
        if let Some(error) = state.fail_attachment_release.clone() {
            return Err(error);
        }
        *released = true;
        state.attached_clients -= 1;
        Ok(())
    }
}

#[async_trait]
impl RoutedSessionHandle for TestHarness {
    async fn attach_client(&self, _context: &Context) -> ChordResult<Arc<dyn RoutedSessionAttachment>> {
        lock(&self.state).attached_clients += 1;
        let harness = self.me.upgrade().ok_or_else(|| ChordError::error("Test harness was dropped"))?;
        Ok(Arc::new(HarnessAttachment { harness, released: Mutex::new(false) }))
    }

    fn terminated(&self) -> Option<BoxFuture<Option<ChordError>>> {
        let termination = self.termination.clone();
        Some(Box::pin(async move { termination.wait().await }))
    }

    async fn close(&self, _context: &Context) -> ChordResult<()> {
        let gate = {
            let mut state = lock(&self.state);
            state.close_count += 1;
            state.close_gate.take()
        };
        if let Some(gate) = gate {
            gate.entered.resolve(());
            gate.release.wait().await;
        }
        if let Some(error) = lock(&self.state).fail_close.take() {
            return Err(error);
        }
        self.closed.resolve(());
        self.termination.resolve(None);
        Ok(())
    }
}

struct ManagementAttachment {
    presentation: RoutedServerPresentation,
}

#[async_trait]
impl RoutedServerServiceAttachment for ManagementAttachment {
    async fn invoke_service(
        &self,
        call: ServiceCall,
        _publish: ServiceUpdatePublisher,
        context: &Context,
    ) -> ChordResult<Option<Json>> {
        if call.instance.is_none() && call.service_id == "pi.session-management" {
            match (call.member.as_str(), call.args.as_slice()) {
                ("attach", [session]) if session.as_str().is_some() => {
                    self.presentation.attach_session(session.as_str().unwrap_or_default(), context).await?;
                    return Ok(Some(Json::Null));
                }
                ("detach", []) => {
                    self.presentation.detach_session(context).await?;
                    return Ok(Some(Json::Null));
                }
                _ => {}
            }
        }
        Err(ChordError::error(format!("Unsupported test server service {}.{}", call.service_id, call.member)))
    }

    async fn release(&self, _context: &Context) -> ChordResult<()> {
        Ok(())
    }
}

struct TestServerServices;

#[async_trait]
impl RoutedServerServiceHost for TestServerServices {
    async fn attach_client(
        &self,
        presentation: RoutedServerPresentation,
        _context: &Context,
    ) -> ChordResult<Arc<dyn RoutedServerServiceAttachment>> {
        Ok(Arc::new(ManagementAttachment { presentation }))
    }
}

/// Server services handling `pi.session-management` `attach(sessionId)` and `detach()`.
pub fn create_test_server_services() -> Arc<dyn RoutedServerServiceHost> {
    Arc::new(TestServerServices)
}

#[derive(Default)]
struct HostState {
    sessions: HashMap<String, BasicSessionMetadata>,
    harnesses: HashMap<String, Vec<Arc<TestHarness>>>,
    open_session_count: usize,
    next_open_session_error: Option<ChordError>,
    next_harness_close_error: Option<ChordError>,
    open_gate: Option<Gate>,
}

/// A host over seeded Session metadata that opens one [`TestHarness`] per Session.
pub struct TestServerHost {
    services: Arc<dyn RoutedServerServiceHost>,
    state: Mutex<HostState>,
}

impl Default for TestServerHost {
    fn default() -> Self {
        Self { services: create_test_server_services(), state: Mutex::new(HostState::default()) }
    }
}

impl TestServerHost {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn seed(&self, id: &str) -> BasicSessionMetadata {
        let metadata = BasicSessionMetadata { id: id.to_string() };
        lock(&self.state).sessions.insert(id.to_string(), metadata.clone());
        metadata
    }

    pub fn gate_next_open_session(&self) -> Gate {
        let gate = Gate::default();
        lock(&self.state).open_gate = Some(gate.clone());
        gate
    }

    pub fn set_next_open_session_error(&self, error: ChordError) {
        lock(&self.state).next_open_session_error = Some(error);
    }

    pub fn set_next_harness_close_error(&self, error: ChordError) {
        lock(&self.state).next_harness_close_error = Some(error);
    }

    pub fn open_session_count(&self) -> usize {
        lock(&self.state).open_session_count
    }

    /// The number of Sessions with at least one opened harness.
    pub fn harness_session_count(&self) -> usize {
        lock(&self.state).harnesses.len()
    }

    pub fn harnesses(&self, id: &str) -> Vec<Arc<TestHarness>> {
        lock(&self.state).harnesses.get(id).cloned().unwrap_or_default()
    }

    pub fn latest_harness(&self, id: &str) -> Arc<TestHarness> {
        self.harnesses(id).last().cloned().unwrap_or_else(|| panic!("No harness for {id}"))
    }
}

#[async_trait]
impl ServerHost for TestServerHost {
    type Metadata = BasicSessionMetadata;

    fn server_services(&self) -> Arc<dyn RoutedServerServiceHost> {
        self.services.clone()
    }

    async fn resolve_session(&self, session_id: &str, _context: &Context) -> ChordResult<BasicSessionMetadata> {
        let metadata = lock(&self.state).sessions.get(session_id).cloned();
        metadata.ok_or_else(|| ServerError::session_not_found(format!("Unknown session: {session_id}")).into())
    }

    async fn open_session(
        &self,
        metadata: BasicSessionMetadata,
        _context: &Context,
    ) -> ChordResult<Arc<dyn RoutedSessionHandle>> {
        let gate = {
            let mut state = lock(&self.state);
            state.open_session_count += 1;
            state.open_gate.take()
        };
        if let Some(gate) = gate {
            gate.entered.resolve(());
            gate.release.wait().await;
        }
        let mut state = lock(&self.state);
        if !state.sessions.contains_key(&metadata.id) {
            return Err(ServerError::session_not_found(format!("Unknown session: {}", metadata.id)).into());
        }
        if let Some(error) = state.next_open_session_error.take() {
            return Err(error);
        }
        let harness = TestHarness::new(metadata.clone());
        lock(&harness.state).fail_close = state.next_harness_close_error.take();
        state.harnesses.entry(metadata.id).or_default().push(harness.clone());
        Ok(harness)
    }
}
