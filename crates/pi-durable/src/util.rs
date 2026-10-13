//! Small helpers the harness shares: pi-ai's `uuidv7`, tool argument validation, and random bytes.

use std::sync::Mutex;

use pi_ai::{JsonObject, Tool, ToolCall};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::Value;

use crate::errors::{DurableError, DurableResult};

const MAX_UUID_V7_TIMESTAMP: u64 = 0xffff_ffff_ffff;
const MAX_SEQUENCE: u64 = (1 << 41) - 1;

struct UuidState {
    last_ordinary_timestamp: Option<u64>,
    sequence: Option<u64>,
}

static UUID_STATE: Mutex<UuidState> = Mutex::new(UuidState { last_ordinary_timestamp: None, sequence: None });

/// Fill `bytes` from the system random source.
pub fn random_bytes(bytes: &mut [u8]) {
    let _ = SystemRandom::new().fill(bytes);
}

/// A uniformly distributed `f64` in `[0, 1)`, like `Math.random()`.
pub fn random_f64() -> f64 {
    let mut bytes = [0u8; 8];
    random_bytes(&mut bytes);
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
}

fn now_ms() -> u64 {
    pi_ai::now_ms().max(0) as u64
}

/// A time-ordered UUIDv7 (upstream pi-ai `uuidv7()`). Ids from one process sort in creation order: the timestamp
/// never goes backwards and a 41-bit counter, seeded randomly, fills the random bits after it.
pub fn uuidv7() -> String {
    generate(None).expect("UUIDv7 generator sequence exhausted")
}

/// A UUIDv7 with the supplied timestamp, preserved as given (upstream `uuidv7(timestampMs)`, for follower ids).
pub fn uuidv7_at(timestamp_ms: u64) -> DurableResult<String> {
    generate(Some(timestamp_ms))
}

fn generate(timestamp_ms: Option<u64>) -> DurableResult<String> {
    let requested = timestamp_ms.unwrap_or_else(now_ms);
    if requested > MAX_UUID_V7_TIMESTAMP {
        return Err(DurableError::error(format!(
            "UUIDv7 timestamp must be an integer between 0 and {MAX_UUID_V7_TIMESTAMP}"
        )));
    }
    let mut bytes = [0u8; 16];
    random_bytes(&mut bytes);
    let mut state = UUID_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let timestamp = match timestamp_ms {
        Some(timestamp) => timestamp,
        None => {
            let effective = state.last_ordinary_timestamp.map_or(requested, |last| requested.max(last));
            state.last_ordinary_timestamp = Some(effective);
            effective
        }
    };
    let sequence = match state.sequence {
        None => bytes[1..6].iter().fold(0u64, |value, byte| (value << 8) | u64::from(*byte)),
        Some(MAX_SEQUENCE) => return Err(DurableError::error("UUIDv7 generator sequence exhausted")),
        Some(sequence) => sequence + 1,
    };
    state.sequence = Some(sequence);
    drop(state);
    bytes[..6].copy_from_slice(&timestamp.to_be_bytes()[2..]);
    bytes[6] = 0x70 | ((sequence >> 37) & 0x0f) as u8;
    bytes[7] = ((sequence >> 29) & 0xff) as u8;
    bytes[8] = 0x80 | ((sequence >> 23) & 0x3f) as u8;
    bytes[9] = ((sequence >> 15) & 0xff) as u8;
    bytes[10] = ((sequence >> 7) & 0xff) as u8;
    bytes[11] = (((sequence & 0x7f) << 1) as u8) | (bytes[11] & 0x01);
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
}

/// Upstream `validateToolArguments(tool, call)`: coerce the call's arguments toward the tool's schema and check
/// them. The error text is pi-ai's, which the model sees as the tool result.
pub fn validate_tool_arguments(tool: &Tool, call: &ToolCall) -> Result<JsonObject, String> {
    let arguments = Value::Object(call.arguments.clone());
    match pi_agent::validate::validate_tool_arguments(&tool.name, &tool.parameters, &arguments)? {
        Value::Object(arguments) => Ok(arguments),
        other => Err(format!("Validation failed for tool \"{}\": arguments must be an object, got {other}", tool.name)),
    }
}
