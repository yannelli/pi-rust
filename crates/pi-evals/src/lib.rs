//! Behavioral evals for the pi coding agent: host evals, the documentation-lift
//! comparison, and the plan and report logic of upstream `@earendil-works/pi-evals`.

pub mod cli;
pub mod container;
pub mod docker;
pub mod evals;
pub mod framework;
pub mod fsutil;
pub mod harness;
mod harness_run;
pub mod plan;
pub mod report;
mod report_format;
mod report_summary;
pub mod session;
