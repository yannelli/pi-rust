//! Extension integration tests: upstream example extensions and temporary extensions run on
//! the embedded JavaScript runtime against sessions over the faux provider.

mod api;
mod events;
mod examples;
mod examples_more;
mod harness;
mod host;
#[path = "../agent_session/http_mock.rs"]
mod http_mock;
mod loading;
mod provider;
mod provider_hooks;
mod runtime;
