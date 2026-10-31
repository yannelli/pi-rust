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
#[path = "../agent_session/mcp_fake.rs"]
#[allow(dead_code)]
mod mcp_fake;
mod native;
mod native_contracts;
mod native_usage;
#[path = "../offline_guard/mod.rs"]
mod offline_guard;
mod provider;
mod provider_compose;
mod provider_hooks;
mod reaches_tools;
mod runtime;
