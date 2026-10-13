//! Keyboard input: legacy sequences, xterm modifyOtherKeys, and the Kitty keyboard protocol.
//! [`matches_key`] tests one input sequence against a key id such as `"ctrl+shift+p"`, and
//! [`parse_key`] returns the key id for a sequence.

use std::sync::atomic::{AtomicBool, Ordering};

mod constants;
mod ids;
mod kitty;
mod matching;
mod parse;

pub use ids::{Key, KeyId};
pub use kitty::{KeyEventType, is_key_release, is_key_repeat, kitty_event_type};
pub use matching::{is_windows_terminal_session, matches_key, matches_key_with_env};
pub use parse::{decode_kitty_printable, decode_printable_key, parse_key, parse_key_with_env};

static KITTY_PROTOCOL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Sets the process-wide Kitty keyboard protocol state. The terminal calls this after it
/// detects protocol support, and with `false` when it restores the terminal.
pub fn set_kitty_protocol_active(active: bool) {
    KITTY_PROTOCOL_ACTIVE.store(active, Ordering::Release);
}

/// Returns true when the Kitty keyboard protocol is active.
pub fn is_kitty_protocol_active() -> bool {
    KITTY_PROTOCOL_ACTIVE.load(Ordering::Acquire)
}
