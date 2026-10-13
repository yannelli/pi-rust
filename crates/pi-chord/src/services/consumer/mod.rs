//! The consumer side of remote services.

mod binding;
pub(crate) mod facade;
pub(crate) mod keyed;

pub use binding::{RemoteServiceBinding, RemoteServiceBindingOptions, create_remote_service_binding};
