//! pi-client talking to pi-server in-process over an ordered byte pipe, with a Chord service
//! provider behind the routed Session.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::services::{
    RemoteServiceEndpoint, RemoteServiceProvider, ServiceCall, ServiceDefinition, ServiceImpl, ServiceMode,
    ServiceProviderUpdate, ServiceUpdatePublisher, create_remote_service_endpoint, define_service,
};
use pi_chord::{BoxFuture, ChordResult, Context, Json, MutableReplicatedState, json, replicated_state};
use pi_client::{
    ByteTransport, ByteTransportFactory, Client, ClientError, ClientOptions, create_client_service_transport,
};
use pi_protocol::RpcTarget;
use pi_server::testing::create_test_server_services;
use pi_server::{
    BasicSessionMetadata, ByteConnection, RoutedServerServiceHost, RoutedSessionAttachment, RoutedSessionHandle,
    Server, ServerError, ServerHost, ServerOptions,
};
use tokio::sync::mpsc;

const SERVER_ID: &str = "00000000-0000-4000-8000-0000000000e2";

enum Chunk {
    Data(Vec<u8>),
    Close,
}

struct PipeEnd {
    sender: mpsc::UnboundedSender<Chunk>,
    closed: Mutex<bool>,
}

impl PipeEnd {
    fn push(&self, chunk: Chunk) -> bool {
        !*self.closed.lock().unwrap() && self.sender.send(chunk).is_ok()
    }
}

impl ByteConnection for PipeEnd {
    fn closed(&self) -> bool {
        *self.closed.lock().unwrap()
    }

    fn send(&self, chunk: Vec<u8>) -> BoxFuture<ChordResult<()>> {
        let sent = self.push(Chunk::Data(chunk));
        Box::pin(async move { if sent { Ok(()) } else { Err(pi_chord::ChordError::error("pipe closed")) } })
    }

    fn close(&self, final_chunk: Option<Vec<u8>>) -> BoxFuture<ChordResult<()>> {
        if let Some(chunk) = final_chunk {
            self.push(Chunk::Data(chunk));
        }
        self.push(Chunk::Close);
        *self.closed.lock().unwrap() = true;
        Box::pin(async { Ok(()) })
    }
}

impl ByteTransport for PipeEnd {
    fn send(&self, chunk: Vec<u8>) -> BoxFuture<Result<(), ClientError>> {
        let sent = self.push(Chunk::Data(chunk));
        Box::pin(async move { if sent { Ok(()) } else { Err(ClientError::Other("pipe closed".into())) } })
    }

    fn close(&self) {
        self.push(Chunk::Close);
        *self.closed.lock().unwrap() = true;
    }
}

/// A transport factory whose every connection is accepted by `server` in-process.
fn in_process<H: ServerHost>(server: Server<H>) -> ByteTransportFactory {
    Arc::new(move |handlers| {
        let (to_server, mut server_inbox) = mpsc::unbounded_channel();
        let (to_client, mut client_inbox) = mpsc::unbounded_channel();
        let server_end = Arc::new(PipeEnd { sender: to_client, closed: Mutex::new(false) });
        let server_handler = server.accept(server_end);
        tokio::spawn(async move {
            while let Some(Chunk::Data(chunk)) = server_inbox.recv().await {
                server_handler.on_data(&chunk);
            }
            server_handler.on_close();
        });
        tokio::spawn(async move {
            while let Some(Chunk::Data(chunk)) = client_inbox.recv().await {
                handlers.on_data(&chunk);
            }
            handlers.on_close();
        });
        let transport: Arc<dyn ByteTransport> = Arc::new(PipeEnd { sender: to_server, closed: Mutex::new(false) });
        Box::pin(async move { Ok(transport) })
    })
}

struct CounterSession {
    provider: RemoteServiceProvider,
}

struct EndpointAttachment(RemoteServiceEndpoint);

#[async_trait]
impl RoutedSessionAttachment for EndpointAttachment {
    async fn invoke_service(
        &self,
        call: ServiceCall,
        publish: ServiceUpdatePublisher,
        ctx: &Context,
    ) -> ChordResult<Option<Json>> {
        self.0.invoke(call, publish, ctx).await
    }

    async fn release(&self, _: &Context) -> ChordResult<()> {
        self.0.dispose();
        Ok(())
    }
}

#[async_trait]
impl RoutedSessionHandle for CounterSession {
    async fn attach_client(&self, _: &Context) -> ChordResult<Arc<dyn RoutedSessionAttachment>> {
        Ok(Arc::new(EndpointAttachment(create_remote_service_endpoint(&self.provider))))
    }

    async fn close(&self, _: &Context) -> ChordResult<()> {
        self.provider.dispose()
    }
}

struct CounterHost {
    state: MutableReplicatedState,
}

#[async_trait]
impl ServerHost for CounterHost {
    type Metadata = BasicSessionMetadata;

    fn server_services(&self) -> Arc<dyn RoutedServerServiceHost> {
        create_test_server_services()
    }

    async fn resolve_session(&self, id: &str, _: &Context) -> ChordResult<BasicSessionMetadata> {
        if id != "session-1" {
            return Err(ServerError::session_not_found(format!("Unknown session: {id}")).into());
        }
        Ok(BasicSessionMetadata { id: id.into() })
    }

    async fn open_session(&self, _: BasicSessionMetadata, _: &Context) -> ChordResult<Arc<dyn RoutedSessionHandle>> {
        let counter = define_service::<()>("test.counter")?;
        let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&counter.erase())])?;
        let state = self.state.clone();
        let implementation = ServiceImpl::new().state("state", &self.state).method("increment", move |_, context| {
            let state = state.clone();
            async move {
                state.change(&context, |draft| {
                    let next = draft["value"].as_f64().unwrap_or(0.0) + 1.0;
                    draft.set("value", next)
                })?;
                Ok(Some(state.value()["value"].clone()))
            }
        });
        provider.provide(&counter.erase(), implementation)?;
        Ok(Arc::new(CounterSession { provider }))
    }
}

#[tokio::test]
async fn client_attaches_calls_and_observes_a_session_through_the_server() {
    let host = Arc::new(CounterHost { state: replicated_state(json!({ "value": 0 })) });
    let server = Server::new(host, ServerOptions::new(SERVER_ID, vec![])).unwrap();
    let client = Client::connect_new(ClientOptions::new(SERVER_ID, in_process(server.clone()))).await.unwrap();
    assert_eq!(client.hello().unwrap().server_id, SERVER_ID);

    let missing = ServiceCall { args: vec![json!("missing")], ..attach_call() };
    let error = client.request(RpcTarget::server(SERVER_ID), missing, None).await.unwrap_err();
    assert_eq!(error.code(), Some("session_not_found"));

    let attach = ServiceCall { args: vec![json!("session-1")], ..attach_call() };
    assert_eq!(client.request(RpcTarget::server(SERVER_ID), attach, None).await.unwrap(), Some(Json::Null));
    let attachment = client.attachment().expect("attachment route published before the response");
    assert_eq!(attachment.session_id, "session-1");

    let resolver = client.clone();
    let transport = create_client_service_transport(&client, Arc::new(move || resolver.attachment().map(Into::into)));
    let updates = Arc::new(Mutex::new(Vec::<ServiceProviderUpdate>::new()));
    let recorded = updates.clone();
    let listener = Arc::new(move |update: ServiceProviderUpdate, _: &Context| {
        recorded.lock().unwrap().push(update);
        Ok(())
    });
    let subscription =
        transport.subscribe("test.counter", ServiceMode::Singleton, listener, &Context::background()).await.unwrap();
    let snapshot = subscription.snapshot().to_json();
    assert_eq!(snapshot["instances"][0]["members"].as_array().unwrap().len(), 2);
    subscription.activate().unwrap();

    let increment =
        ServiceCall { service_id: "test.counter".into(), instance: None, member: "increment".into(), args: vec![] };
    assert_eq!(transport.invoke(increment, &Context::background()).await.unwrap(), Some(json!(1)));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while updates.lock().unwrap().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "update was not delivered");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let update = updates.lock().unwrap()[0].to_json();
    assert_eq!(update, json!({ "type": "state", "member": "state", "sequence": 1, "ops": [["s", ["value"], 1]] }));

    subscription.close(&Context::background()).await.unwrap();
    client.dispose();
    server.close().await.unwrap();
}

fn attach_call() -> ServiceCall {
    ServiceCall { service_id: "pi.session-management".into(), instance: None, member: "attach".into(), args: vec![] }
}
