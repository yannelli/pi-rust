//! Run limits (pi-rust extension): `--max-turns` stops a run after that many assistant
//! responses, and `--max-cost` once the session's total cost, as the footer sums it, reaches
//! the limit. A stop aborts the run the way Esc does, after the last turn's tool results.

use std::sync::Mutex;

use pi_ai::{AssistantMessage, Message, StopReason};
use serde_json::{Value, json};

use super::{AgentSession, AgentSessionEvent, lock};
use crate::session::usage::usage_totals;

/// Exit code of print and JSON mode when a run limit stopped the run.
pub const RUN_LIMIT_EXIT_CODE: i32 = 3;

/// The limits of `--max-turns` and `--max-cost`; `None` means no limit.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RunLimits {
    /// Assistant responses per prompt; tool results do not count.
    pub max_turns: Option<u32>,
    /// Total session cost in US dollars.
    pub max_cost: Option<f64>,
}

impl RunLimits {
    /// A `--max-turns` value: a positive integer.
    pub fn parse_max_turns(value: &str) -> Result<u32, String> {
        match value.trim().parse::<u32>() {
            Ok(turns) if turns > 0 => Ok(turns),
            _ => Err(format!("--max-turns requires a positive integer, got \"{value}\"")),
        }
    }

    /// A `--max-cost` value: a non-negative amount in US dollars.
    pub fn parse_max_cost(value: &str) -> Result<f64, String> {
        match value.trim().parse::<f64>() {
            Ok(cost) if cost.is_finite() && cost >= 0.0 => Ok(cost.abs()),
            _ => Err(format!("--max-cost requires a non-negative number of US dollars, got \"{value}\"")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunLimitKind {
    Turns,
    Cost,
}

/// A run a limit stopped: the limit, the turns of the run, and the session cost.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RunLimitStop {
    pub kind: RunLimitKind,
    pub turns: u32,
    pub cost: f64,
    pub limits: RunLimits,
}

impl RunLimitStop {
    /// For example `stopped after 10 turns (--max-turns)`.
    pub fn message(&self) -> String {
        match self.kind {
            RunLimitKind::Turns => {
                let plural = if self.turns == 1 { "" } else { "s" };
                format!("stopped after {} turn{plural} (--max-turns)", self.turns)
            }
            RunLimitKind::Cost => {
                let max = self.limits.max_cost.unwrap_or_default();
                format!("stopped at ${:.4} session cost (--max-cost {max})", self.cost)
            }
        }
    }

    /// The `run_limit` event of JSON and RPC mode.
    pub fn to_json(&self) -> Value {
        let (limit, max) = match self.kind {
            RunLimitKind::Turns => ("maxTurns", json!(self.limits.max_turns)),
            RunLimitKind::Cost => ("maxCost", json!(self.limits.max_cost)),
        };
        json!({
            "type": "run_limit",
            "limit": limit,
            "max": max,
            "turns": self.turns,
            "cost": self.cost,
            "message": self.message(),
        })
    }
}

#[derive(Default)]
struct Progress {
    turns: u32,
    stop: Option<RunLimitStop>,
}

/// The limits of one session, the turns of its current run, and the last stop.
#[derive(Default)]
pub(crate) struct RunLimitState {
    limits: RunLimits,
    progress: Mutex<Progress>,
}

impl RunLimitState {
    pub(crate) fn new(limits: RunLimits) -> Self {
        Self { limits, progress: Mutex::default() }
    }
}

impl AgentSession {
    pub fn run_limits(&self) -> RunLimits {
        self.run_limits.limits
    }

    /// The last stop a run limit caused in this session.
    pub fn run_limit_stop(&self) -> Option<RunLimitStop> {
        lock(&self.run_limits.progress).stop
    }

    pub(crate) fn reset_run_turns(&self) {
        lock(&self.run_limits.progress).turns = 0;
    }

    fn session_cost(&self) -> f64 {
        usage_totals(self.session_manager().entries()).cost
    }

    fn record_limit_stop(&self, kind: RunLimitKind, turns: u32) {
        let stop = RunLimitStop { kind, turns, cost: self.session_cost(), limits: self.run_limits.limits };
        lock(&self.run_limits.progress).stop = Some(stop);
        self.emit(&AgentSessionEvent::RunLimit(stop));
    }

    /// Whether the session cost already reached `--max-cost`, so a prompt must not start a run.
    pub(crate) fn cost_limit_reached(&self) -> bool {
        let Some(max) = self.run_limits.limits.max_cost else { return false };
        if self.session_cost() < max {
            return false;
        }
        self.record_limit_stop(RunLimitKind::Cost, 0);
        true
    }

    /// At `turn_end`: count the response, and stop the run when a limit is reached and the
    /// run would go on (tool calls, queued messages, or a retry).
    pub(crate) fn check_run_limits(&self, message: &Message) {
        let limits = self.run_limits.limits;
        if limits == RunLimits::default() {
            return;
        }
        let turns = {
            let mut progress = lock(&self.run_limits.progress);
            progress.turns += 1;
            progress.turns
        };
        let Message::Assistant(message) = message else { return };
        if !self.run_would_continue(message) {
            return;
        }
        let kind = if limits.max_turns.is_some_and(|max| turns >= max) {
            RunLimitKind::Turns
        } else if limits.max_cost.is_some_and(|max| self.session_cost() >= max) {
            RunLimitKind::Cost
        } else {
            return;
        };
        self.record_limit_stop(kind, turns);
        self.request_abort();
    }

    fn run_would_continue(&self, message: &AssistantMessage) -> bool {
        match message.stop_reason {
            StopReason::Aborted => false,
            StopReason::Error => self.will_retry_after_agent_end(&[Message::Assistant(message.clone())]),
            _ => message.tool_calls().next().is_some() || self.agent.has_queued_messages(),
        }
    }
}
