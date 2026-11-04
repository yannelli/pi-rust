//! Extension integration tests: upstream example extensions and temporary extensions run on
//! the embedded JavaScript runtime against sessions over the faux provider.

mod api;
mod component_capture;
mod events;
mod examples;
mod examples_more;
mod exec;
pub(crate) mod harness;
mod hook_events;
mod host;
mod loading;
mod mcp_call;
mod mcp_elicitation;
mod native;
mod native_contracts;
mod native_usage;
mod provider;
mod provider_compose;
mod provider_hooks;
mod reaches_tools;
mod runtime;

use crate::agent_session::{http_mock, mcp_fake};
use crate::interactive::watchdog;
