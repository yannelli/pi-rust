//! The peer's SETTINGS frames, read from the bytes fed to an HTTP/2 session. h2 keeps them to
//! itself, and `remoteSettings` and `pushAllowed` need them.

/// The client connection preface that comes before a server's first frame.
const PREFACE: usize = 24;
const HEADER: usize = 9;
const SETTINGS: u8 = 4;
const ACK: u8 = 1;

pub(super) struct SettingsScanner {
    preface: usize,
    header: Vec<u8>,
    /// Payload bytes left in the current frame; a SETTINGS payload is also kept.
    remaining: usize,
    payload: Option<Vec<u8>>,
}

impl SettingsScanner {
    pub(super) fn new(server: bool) -> Self {
        Self { preface: if server { PREFACE } else { 0 }, header: Vec::new(), remaining: 0, payload: None }
    }

    /// Follow the frames in `bytes` and return each complete SETTINGS frame (not an ACK) as a
    /// flat list of identifiers and values.
    pub(super) fn scan(&mut self, mut bytes: &[u8]) -> Vec<Vec<u32>> {
        let mut frames = Vec::new();
        while !bytes.is_empty() {
            if self.preface > 0 {
                let skip = self.preface.min(bytes.len());
                self.preface -= skip;
                bytes = &bytes[skip..];
            } else if self.remaining > 0 {
                let take = self.remaining.min(bytes.len());
                if let Some(payload) = &mut self.payload {
                    payload.extend_from_slice(&bytes[..take]);
                }
                self.remaining -= take;
                bytes = &bytes[take..];
                if self.remaining == 0
                    && let Some(payload) = self.payload.take()
                {
                    frames.push(entries(&payload));
                }
            } else {
                let take = (HEADER - self.header.len()).min(bytes.len());
                self.header.extend_from_slice(&bytes[..take]);
                bytes = &bytes[take..];
                if self.header.len() == HEADER {
                    let header = std::mem::take(&mut self.header);
                    let length = usize::from(header[0]) << 16 | usize::from(header[1]) << 8 | usize::from(header[2]);
                    let settings = header[3] == SETTINGS && header[4] & ACK == 0;
                    self.remaining = length;
                    self.payload = settings.then(Vec::new);
                    if settings && length == 0 {
                        frames.push(Vec::new());
                        self.payload = None;
                    }
                }
            }
        }
        frames
    }
}

fn entries(payload: &[u8]) -> Vec<u32> {
    let mut list = Vec::new();
    for entry in payload.chunks_exact(6) {
        list.push(u32::from(u16::from_be_bytes([entry[0], entry[1]])));
        list.push(u32::from_be_bytes([entry[2], entry[3], entry[4], entry[5]]));
    }
    list
}
