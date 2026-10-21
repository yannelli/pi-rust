//! SETTINGS frames on the bytes of an HTTP/2 session. h2 keeps settings to itself, so this reads
//! the peer's frames for `remoteSettings`, adds custom settings to h2's first frame, and sends
//! the frames of `session.settings()`, whose ACKs it takes out before h2 sees them.

use std::collections::VecDeque;

/// The client connection preface that comes before the client's first frame.
const PREFACE: usize = 24;
const HEADER: usize = 9;
const SETTINGS: u8 = 4;
const ACK: u8 = 1;

/// What the peer sent: a SETTINGS frame as a flat list of identifiers and values, or the ACK
/// of h2's first SETTINGS frame or of a frame from `session.settings()`.
pub(super) enum Seen {
    Settings(Vec<u32>),
    Ack,
}

/// Who sent a SETTINGS frame that waits for its ACK.
enum Origin {
    H2 { first: bool },
    Session,
}

/// Where one direction of the connection is: in the client preface, in a frame header, or in
/// a payload with `remaining` bytes left.
struct Position {
    preface: usize,
    header: Vec<u8>,
    remaining: usize,
}

impl Position {
    fn new(preface: bool) -> Self {
        Self { preface: if preface { PREFACE } else { 0 }, header: Vec::new(), remaining: 0 }
    }

    /// Move past the preface or a payload, copying those bytes to `out`.
    fn pass<'a>(&mut self, bytes: &'a [u8], out: &mut Vec<u8>) -> &'a [u8] {
        let left = if self.preface > 0 { &mut self.preface } else { &mut self.remaining };
        let take = (*left).min(bytes.len());
        *left -= take;
        out.extend_from_slice(&bytes[..take]);
        &bytes[take..]
    }

    /// Collect a frame header; it is returned once all nine bytes are in.
    fn header<'a>(&mut self, bytes: &'a [u8]) -> (Option<Vec<u8>>, &'a [u8]) {
        let take = (HEADER - self.header.len()).min(bytes.len());
        self.header.extend_from_slice(&bytes[..take]);
        let header = (self.header.len() == HEADER).then(|| std::mem::take(&mut self.header));
        (header, &bytes[take..])
    }

    fn in_frame(&self) -> bool {
        self.preface > 0 || self.remaining > 0
    }
}

pub(super) struct SettingsTracker {
    input: Position,
    output: Position,
    /// The payload of the peer's SETTINGS frame being read.
    payload: Option<Vec<u8>>,
    /// Frames sent and not yet acknowledged, in order, as a peer acknowledges them in order.
    origins: VecDeque<Origin>,
    /// Custom entries for h2's first SETTINGS frame, moved to `append` while it passes.
    custom: Vec<u8>,
    append: Vec<u8>,
    /// Whether h2 has sent its first SETTINGS frame.
    started: bool,
    /// ACKs h2 owes for the peer's SETTINGS frames. Like nghttp2, they go before new frames.
    owed: usize,
    /// Frames from `session.settings()` that wait for h2 to finish a frame or send its ACKs.
    queued: Vec<Vec<u8>>,
}

impl SettingsTracker {
    pub(super) fn new(server: bool, custom: &[u32]) -> Self {
        Self {
            input: Position::new(server),
            output: Position::new(!server),
            payload: None,
            origins: VecDeque::new(),
            custom: pairs(custom),
            append: Vec::new(),
            started: false,
            owed: 0,
            queued: Vec::new(),
        }
    }

    /// Read the peer's bytes: returns the bytes for h2 and the SETTINGS frames and ACKs in them.
    /// A frame header is held back until it is complete.
    pub(super) fn incoming(&mut self, mut bytes: &[u8]) -> (Vec<u8>, Vec<Seen>) {
        let mut pass = Vec::with_capacity(bytes.len());
        let mut seen = Vec::new();
        while !bytes.is_empty() {
            if self.input.in_frame() {
                let start = pass.len();
                bytes = self.input.pass(bytes, &mut pass);
                if let Some(payload) = &mut self.payload {
                    payload.extend_from_slice(&pass[start..]);
                }
                if self.input.remaining == 0
                    && let Some(payload) = self.payload.take()
                {
                    seen.push(Seen::Settings(entries(&payload)));
                }
                continue;
            }
            let (header, rest) = self.input.header(bytes);
            bytes = rest;
            let Some(header) = header else { break };
            let length = frame_length(&header);
            self.input.remaining = length;
            if header[3] == SETTINGS && header[4] & ACK != 0 && length == 0 {
                match self.origins.pop_front() {
                    // h2 never sent this frame, so its ACK stops here.
                    Some(Origin::Session) => {
                        seen.push(Seen::Ack);
                        continue;
                    }
                    Some(Origin::H2 { first: true }) => seen.push(Seen::Ack),
                    _ => {}
                }
            } else if header[3] == SETTINGS && header[4] & ACK == 0 {
                self.owed += 1;
                if length == 0 {
                    seen.push(Seen::Settings(Vec::new()));
                } else {
                    self.payload = Some(Vec::new());
                }
            }
            pass.extend_from_slice(&header);
        }
        (pass, seen)
    }

    /// Read h2's bytes on their way to the peer: returns them with custom entries added to the
    /// first SETTINGS frame, and with queued frames after the frame that held them up.
    pub(super) fn outgoing(&mut self, mut bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len() + self.custom.len());
        while !bytes.is_empty() {
            if self.output.in_frame() {
                let preface = self.output.preface > 0;
                bytes = self.output.pass(bytes, &mut out);
                if !preface && self.output.remaining == 0 {
                    self.frame_end(&mut out);
                }
                continue;
            }
            let (header, rest) = self.output.header(bytes);
            bytes = rest;
            let Some(mut header) = header else { break };
            let length = frame_length(&header);
            if header[3] == SETTINGS && header[4] & ACK != 0 {
                self.owed = self.owed.saturating_sub(1);
            } else if header[3] == SETTINGS {
                let first = !self.started;
                self.started = true;
                self.origins.push_back(Origin::H2 { first });
                if first && !self.custom.is_empty() {
                    self.append = std::mem::take(&mut self.custom);
                    let total = u32::try_from(length + self.append.len()).unwrap_or(u32::MAX);
                    header[..3].copy_from_slice(&total.to_be_bytes()[1..]);
                }
            }
            out.extend_from_slice(&header);
            self.output.remaining = length;
            if length == 0 {
                self.frame_end(&mut out);
            }
        }
        out
    }

    /// Whether a frame from `session.settings()` can go out at the next frame boundary.
    fn ready(&self) -> bool {
        self.started && self.owed == 0
    }

    fn frame_end(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.append);
        if self.ready() {
            for frame in self.queued.drain(..) {
                self.origins.push_back(Origin::Session);
                out.extend_from_slice(&frame);
            }
        }
    }

    /// A SETTINGS frame with `entries` for `session.settings()`: the bytes to send now, or none
    /// when h2 is inside a frame and it goes after that frame.
    pub(super) fn send(&mut self, entries: &[u32]) -> Option<Vec<u8>> {
        let payload = pairs(entries);
        let mut frame = u32::try_from(payload.len()).unwrap_or(0).to_be_bytes()[1..].to_vec();
        frame.extend_from_slice(&[SETTINGS, 0, 0, 0, 0, 0]);
        frame.extend_from_slice(&payload);
        if self.ready() && !self.output.in_frame() {
            self.origins.push_back(Origin::Session);
            Some(frame)
        } else {
            self.queued.push(frame);
            None
        }
    }
}

fn frame_length(header: &[u8]) -> usize {
    usize::from(header[0]) << 16 | usize::from(header[1]) << 8 | usize::from(header[2])
}

fn pairs(entries: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for pair in entries.chunks_exact(2) {
        if let Ok(id) = u16::try_from(pair[0]) {
            bytes.extend_from_slice(&id.to_be_bytes());
            bytes.extend_from_slice(&pair[1].to_be_bytes());
        }
    }
    bytes
}

fn entries(payload: &[u8]) -> Vec<u32> {
    let mut list = Vec::new();
    for entry in payload.chunks_exact(6) {
        list.push(u32::from(u16::from_be_bytes([entry[0], entry[1]])));
        list.push(u32::from_be_bytes([entry[2], entry[3], entry[4], entry[5]]));
    }
    list
}
