//! Conformance suites and benchmarks that custom storages and environments run against themselves.

pub mod storage_conformance;

#[doc(hidden)]
pub use tokio as __tokio;
pub mod faux;
pub mod storage_benchmark;
