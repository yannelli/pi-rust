//! Input-side state of [`super::ProcessTerminal`]: stdin buffering, keyboard protocol negotiation,
//! and program status support detection. Runs on the input thread under a mutex.

use std::time::{Duration, Instant};

use crate::keys::set_kitty_protocol_active;
use crate::native_platform::{ModifierKey, is_native_modifier_pressed};
use crate::program_status::{
    PROGRAM_STATUS_QUERY, ProgramState, ProgramStatus, format_program_status, is_program_status_reply,
};
use crate::stdin_buffer::{StdinBuffer, StdinBufferEvent, StdinBufferOptions};
use crate::terminal::{
    KeyboardProtocolNegotiationSequence, is_keyboard_protocol_negotiation_sequence_prefix,
    normalize_native_shift_enter_input, parse_keyboard_protocol_negotiation_sequence,
};

const DESIRED_KITTY_KEYBOARD_PROTOCOL_FLAGS: u32 = 7;
const KEYBOARD_PROTOCOL_RESPONSE_FRAGMENT_TIMEOUT: Duration = Duration::from_millis(150);
const DEVICE_ATTRIBUTES_QUERY: &str = "\x1b[c";
pub(super) const TERMINAL_PROGRESS_ACTIVE_SEQUENCE: &str = "\x1b]9;4;3\x07";
pub(super) const TERMINAL_PROGRESS_CLEAR_SEQUENCE: &str = "\x1b]9;4;0\x07";

pub(super) type InputHandler = Box<dyn FnMut(String) + Send>;
pub(super) type OutputFn = Box<dyn FnMut(&str) + Send>;

/// Shared terminal state used by the UI thread and the input thread.
pub(super) struct Core {
    pub output: OutputFn,
    pub handler: Option<InputHandler>,
    pub stdin_buffer: Option<StdinBuffer>,
    pub escape_timeout: Duration,
    pub kitty_active: bool,
    pub modify_other_keys_active: bool,
    pub keyboard_protocol_pushed: bool,
    /// DA1 replies owed to keyboard protocol queries. Later DA1 replies answer other queries.
    pub pending_device_attributes: u32,
    pub negotiation_buffer: String,
    pub negotiation_flush_at: Option<Instant>,
    pub program_status: Option<ProgramStatus>,
    pub program_status_supported: bool,
    pub program_status_query_pending: bool,
    pub detect_native_shift_enter: bool,
    pub draining: bool,
    pub last_data_at: Instant,
}

impl Core {
    pub fn new(output: OutputFn, escape_timeout: Duration, detect_native_shift_enter: bool) -> Self {
        Self {
            output,
            handler: None,
            stdin_buffer: None,
            escape_timeout,
            kitty_active: false,
            modify_other_keys_active: false,
            keyboard_protocol_pushed: false,
            pending_device_attributes: 0,
            negotiation_buffer: String::new(),
            negotiation_flush_at: None,
            program_status: None,
            program_status_supported: false,
            program_status_query_pending: false,
            detect_native_shift_enter,
            draining: false,
            last_data_at: Instant::now(),
        }
    }

    pub fn write(&mut self, data: &str) {
        (self.output)(data);
    }

    /// Sets up the stdin buffer and sends the Kitty, program status, and DA1 queries.
    pub fn query_and_enable_kitty_protocol(&mut self, program_status_override: Option<&str>) {
        self.stdin_buffer = Some(StdinBuffer::new(StdinBufferOptions {
            escape_timeout: self.escape_timeout,
            ..StdinBufferOptions::default()
        }));
        self.keyboard_protocol_pushed = true;
        self.pending_device_attributes += 1;
        self.clear_negotiation_buffer();
        self.program_status_supported = program_status_override == Some("1");
        self.program_status_query_pending = !matches!(program_status_override, Some("1") | Some("0"));
        let status_query = if self.program_status_query_pending { PROGRAM_STATUS_QUERY } else { "" };
        let query =
            format!("\x1b[>{DESIRED_KITTY_KEYBOARD_PROTOCOL_FLAGS}u\x1b[?u{status_query}{DEVICE_ATTRIBUTES_QUERY}");
        self.write(&query);
        self.write_program_status();
    }

    /// Feeds raw stdin text as it arrives.
    pub fn feed(&mut self, data: &str, now: Instant) {
        self.last_data_at = now;
        if self.draining {
            return;
        }
        let Some(buffer) = self.stdin_buffer.as_mut() else { return };
        let events = buffer.process(data, now);
        self.handle_buffer_events(events, now);
    }

    /// Fires due timeouts of the stdin buffer and the negotiation fragment buffer.
    pub fn poll_timers(&mut self, now: Instant) {
        if let Some(buffer) = self.stdin_buffer.as_mut() {
            let events = buffer.on_timeout(now);
            self.handle_buffer_events(events, now);
        }
        if self.negotiation_flush_at.is_some_and(|at| at <= now) {
            self.negotiation_flush_at = None;
            self.flush_negotiation_buffer_as_input();
        }
    }

    /// Earliest pending timeout.
    pub fn next_deadline(&self) -> Option<Instant> {
        let buffer = self.stdin_buffer.as_ref().and_then(StdinBuffer::deadline);
        match (buffer, self.negotiation_flush_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn handle_buffer_events(&mut self, events: Vec<StdinBufferEvent>, now: Instant) {
        for event in events {
            match event {
                StdinBufferEvent::Data(sequence) => self.handle_sequence(sequence, now),
                StdinBufferEvent::Paste(content) => {
                    if let Some(handler) = self.handler.as_mut() {
                        handler(format!("\x1b[200~{content}\x1b[201~"));
                    }
                }
            }
        }
    }

    fn handle_sequence(&mut self, sequence: String, now: Instant) {
        if is_program_status_reply(&sequence) {
            if self.program_status_query_pending {
                self.program_status_query_pending = false;
                self.program_status_supported = true;
                self.write_program_status();
            }
            return;
        }
        match self.read_negotiation_sequence(sequence) {
            Negotiation::Pending => {
                if !self.negotiation_buffer.is_empty() && self.negotiation_flush_at.is_none() {
                    self.negotiation_flush_at = Some(now + KEYBOARD_PROTOCOL_RESPONSE_FRAGMENT_TIMEOUT);
                }
            }
            Negotiation::Parsed(parsed, full) => {
                if !self.handle_negotiation(parsed) {
                    self.forward(full);
                }
            }
            Negotiation::None(sequence) => self.forward(sequence),
        }
    }

    fn read_negotiation_sequence(&mut self, sequence: String) -> Negotiation {
        if !self.negotiation_buffer.is_empty() {
            let buffered = format!("{}{}", self.negotiation_buffer, sequence);
            if let Some(parsed) = parse_keyboard_protocol_negotiation_sequence(&buffered) {
                self.clear_negotiation_buffer();
                return Negotiation::Parsed(parsed, buffered);
            }
            if is_keyboard_protocol_negotiation_sequence_prefix(&buffered) {
                self.set_negotiation_buffer(buffered);
                return Negotiation::Pending;
            }
            self.flush_negotiation_buffer_as_input();
        }
        if let Some(parsed) = parse_keyboard_protocol_negotiation_sequence(&sequence) {
            return Negotiation::Parsed(parsed, sequence);
        }
        if is_keyboard_protocol_negotiation_sequence_prefix(&sequence) {
            self.set_negotiation_buffer(sequence);
            return Negotiation::Pending;
        }
        Negotiation::None(sequence)
    }

    fn handle_negotiation(&mut self, sequence: KeyboardProtocolNegotiationSequence) -> bool {
        self.clear_negotiation_buffer();
        match sequence {
            KeyboardProtocolNegotiationSequence::DeviceAttributes => {
                if self.pending_device_attributes == 0 {
                    return false;
                }
                self.pending_device_attributes -= 1;
                // The last owed DA answers the latest query, which got no program status reply first.
                if self.pending_device_attributes == 0 {
                    self.program_status_query_pending = false;
                }
                if !self.kitty_active {
                    self.enable_modify_other_keys();
                }
                true
            }
            KeyboardProtocolNegotiationSequence::KittyFlags(flags) => {
                if flags != 0 {
                    self.disable_modify_other_keys();
                    if !self.kitty_active {
                        self.kitty_active = true;
                        set_kitty_protocol_active(true);
                    }
                } else {
                    self.enable_modify_other_keys();
                }
                true
            }
        }
    }

    fn set_negotiation_buffer(&mut self, sequence: String) {
        self.negotiation_flush_at = None;
        self.negotiation_buffer = sequence;
    }

    pub fn clear_negotiation_buffer(&mut self) {
        self.negotiation_flush_at = None;
        self.negotiation_buffer.clear();
    }

    fn flush_negotiation_buffer_as_input(&mut self) {
        if self.negotiation_buffer.is_empty() {
            return;
        }
        let sequence = std::mem::take(&mut self.negotiation_buffer);
        self.clear_negotiation_buffer();
        self.forward(sequence);
    }

    fn forward(&mut self, sequence: String) {
        if self.handler.is_none() {
            return;
        }
        let detect = sequence == "\r" && self.detect_native_shift_enter;
        let shift = detect && is_native_modifier_pressed(ModifierKey::Shift);
        let input = normalize_native_shift_enter_input(&sequence, detect, shift);
        if let Some(handler) = self.handler.as_mut() {
            handler(input);
        }
    }

    pub fn enable_modify_other_keys(&mut self) {
        if self.kitty_active || self.modify_other_keys_active {
            return;
        }
        self.write("\x1b[>4;2m");
        self.modify_other_keys_active = true;
    }

    pub fn disable_modify_other_keys(&mut self) {
        if !self.modify_other_keys_active {
            return;
        }
        self.write("\x1b[>4;0m");
        self.modify_other_keys_active = false;
    }

    /// Pops the Kitty keyboard flags if they were pushed or confirmed.
    pub fn disable_kitty_protocol(&mut self) {
        if self.keyboard_protocol_pushed || self.kitty_active {
            self.write("\x1b[<u");
            self.keyboard_protocol_pushed = false;
            self.kitty_active = false;
            set_kitty_protocol_active(false);
        }
    }

    pub fn set_program_status(&mut self, status: &ProgramStatus) {
        self.program_status = (status.state != ProgramState::Clear).then(|| status.clone());
        if self.program_status_supported {
            let encoded = format_program_status(status);
            self.write(&encoded);
        }
    }

    pub fn write_program_status(&mut self) {
        if self.program_status_supported
            && let Some(status) = self.program_status.clone()
        {
            let encoded = format_program_status(&status);
            self.write(&encoded);
        }
    }
}

enum Negotiation {
    Pending,
    Parsed(KeyboardProtocolNegotiationSequence, String),
    None(String),
}
