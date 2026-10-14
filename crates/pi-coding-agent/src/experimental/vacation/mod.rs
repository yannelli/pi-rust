//! Port of upstream `vacation/`: a durable vacation planner on the durable coding agent's runtime and TUI.
//! `harness-setup.ts`, `runtime.ts`, `sessions.ts`, `main.ts`, and `tui.ts` are shared with `durable/`.

#[allow(clippy::module_inception)]
mod vacation;

use pi_durable::DurableResult;
use pi_durable::harness::{Registry, create_registry};

pub use vacation::{RESEARCH, SEARCH_EXTENSION, VACATION_EXTENSION, search_extension, vacation_extension};

/// A registry with the vacation planner and its research subagent's search. No coding tools, no pi prompt.
pub fn create_vacation_registry() -> DurableResult<Registry> {
    let registry = create_registry();
    registry.install(vacation_extension())?;
    registry.install(search_extension())?;
    Ok(registry)
}
