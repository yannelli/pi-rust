//! The integration tests, built as one test binary.

#[path = "acme-server.rs"]
mod acme_server;
mod comparison;
#[path = "configured-runtime.rs"]
mod configured_runtime;
mod harness;
mod plan;
mod report;
