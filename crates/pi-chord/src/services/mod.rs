//! Remote services: tokens, providers, consumer bindings, transports, and the wire format.

pub mod consumer;
mod endpoint;
pub(crate) mod handle;
pub(crate) mod instances;
mod provider;
mod provider_subscriber;
mod remote;
mod state_codec;
mod token;
mod transport;
mod validate;
mod wire;
mod wire_parse;

pub use consumer::{RemoteServiceBinding, RemoteServiceBindingOptions, create_remote_service_binding};
pub use endpoint::{RemoteServiceEndpoint, ServiceUpdatePublisher, create_remote_service_endpoint};
pub use handle::{AccessCheck, LocalHandle, ServiceHandle, StateHandle};
pub use provider::{RemoteServiceProvider, ServiceDefinition};
pub use provider_subscriber::{ProviderSubscription, ServiceUpdateListener};
pub use remote::{
    ObserveHandler, RemoteServiceSource, RemoteServiceSourceOpenOptions, RemoteServices, observe_handler,
};
pub use state_codec::{
    ServiceStateDecoder, ServiceStateEncoder, create_service_state_decoder, create_service_state_encoder,
};
pub use token::{
    MemberKind, MethodFn, Service, ServiceImpl, ServiceMember, ServiceRef, define_local_service, define_service,
};
pub use transport::{
    LoopbackServiceTransport, RemoteServiceTransport, ServiceSubscription, create_loopback_service_transport,
};
pub use wire::*;
pub use wire_parse::{
    ServiceControlCall, create_service_catalogue_call, create_service_subscribe_call, create_service_unsubscribe_call,
    decode_service_control_call, parse_address, parse_service_call, parse_service_catalogue,
    parse_service_provider_update, parse_service_subscription_snapshot, parse_wire_service_provider_update,
    parse_wire_service_subscription_snapshot,
};
