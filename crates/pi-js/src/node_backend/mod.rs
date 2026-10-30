//! The Node backend: JavaScript runs in a `worker_threads` Worker of a `node` child process,
//! driven over a loopback TCP connection with one JSON message per line.

mod bootstrap;
mod bridge;
pub(crate) mod client;
mod executable;
mod modules;
pub(crate) mod ops;
mod transport;
mod wire;

pub(crate) use bootstrap::start;
pub use executable::{MIN_NODE_MAJOR, NODE_PATH_ENV, NodeVersion, find_executable, probe};
