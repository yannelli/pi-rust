//! Runtime defaults shared by settings accessors (upstream `core/settings-defaults.ts`).

use pi_ai::ThinkingLevel;

/// Thinking level when neither the session, the model, nor settings choose one.
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;
pub const COMPACTION_ENABLED: bool = true;
pub const COMPACTION_RESERVE_TOKENS: u64 = 16_384;
pub const COMPACTION_KEEP_RECENT_TOKENS: u64 = 20_000;
pub const BRANCH_SUMMARY_RESERVE_TOKENS: u64 = 16_384;
pub const RETRY_ENABLED: bool = true;
pub const RETRY_MAX_RETRIES: u32 = 3;
pub const RETRY_BASE_DELAY_MS: u64 = 2_000;
pub const RETRY_MAX_AGENT_DELAY_MS: u64 = 60_000;
pub const PROVIDER_MAX_RETRY_DELAY_MS: u64 = 60_000;
pub const DEFAULT_HTTP_IDLE_TIMEOUT_MS: u64 = 300_000;
/// `terminal.notifyMinSeconds` (pi-rust extension).
pub const NOTIFY_MIN_SECONDS: f64 = 10.0;
