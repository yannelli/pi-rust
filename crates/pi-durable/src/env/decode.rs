//! UTF-8 decoding that matches decoding the whole input at once (WHATWG `TextDecoder`) when the input arrives in
//! pieces. Invalid sequences become U+FFFD per maximal subpart, and an incomplete trailing sequence is held back until
//! the next piece or the end of the stream.

/// Whether decoding the whole input drops its first three bytes as a byte-order mark.
pub fn starts_with_bom(first_bytes: &[u8]) -> bool {
    first_bytes.starts_with(&[0xef, 0xbb, 0xbf])
}

/// Decodes one stream chunk by chunk exactly like decoding all of it at once.
#[derive(Clone, Debug)]
pub struct StreamDecoder {
    pending: Vec<u8>,
    strip_bom: bool,
    started: bool,
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    /// A decoder of a whole stream: a leading byte-order mark is dropped, like `new TextDecoder().decode(bytes)`.
    pub fn new() -> Self {
        Self { pending: Vec::new(), strip_bom: true, started: false }
    }

    /// A decoder of a byte range (upstream `rangeDecoder()`): every U+FEFF is kept. Callers that start at the beginning
    /// of a file skip a leading mark themselves.
    pub fn range() -> Self {
        Self { pending: Vec::new(), strip_bom: false, started: false }
    }

    /// Text for `bytes`, holding back an incomplete character.
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        if bytes.is_empty() {
            return String::new();
        }
        let text = if self.pending.is_empty() {
            let cut = complete_prefix(bytes);
            self.pending.extend_from_slice(&bytes[cut..]);
            String::from_utf8_lossy(&bytes[..cut]).into_owned()
        } else {
            let mut data = std::mem::take(&mut self.pending);
            data.extend_from_slice(bytes);
            let cut = complete_prefix(&data);
            let text = String::from_utf8_lossy(&data[..cut]).into_owned();
            self.pending = data.split_off(cut);
            text
        };
        self.leading(text)
    }

    /// The end of the stream: an incomplete character still held back becomes U+FFFD.
    pub fn finish(&mut self) -> String {
        let pending = std::mem::take(&mut self.pending);
        let text = String::from_utf8_lossy(&pending).into_owned();
        self.leading(text)
    }

    fn leading(&mut self, text: String) -> String {
        if self.started || text.is_empty() {
            return text;
        }
        self.started = true;
        // U+FEFF encodes only as EF BB BF, so a leading U+FEFF is exactly a byte-order mark at the stream's start.
        match text.strip_prefix('\u{feff}') {
            Some(rest) if self.strip_bom => rest.to_string(),
            _ => text,
        }
    }
}

/// Length of the longest prefix of `bytes` that does not end inside a character that later bytes could complete.
fn complete_prefix(bytes: &[u8]) -> usize {
    let mut end = 0;
    for chunk in bytes.utf8_chunks() {
        let invalid = chunk.invalid();
        let chunk_end = end + chunk.valid().len() + invalid.len();
        if chunk_end == bytes.len() && !invalid.is_empty() && is_incomplete(invalid) {
            return end + chunk.valid().len();
        }
        end = chunk_end;
    }
    end
}

/// Whether `bytes` is the start of a valid character that is cut off by the end of the input.
fn is_incomplete(bytes: &[u8]) -> bool {
    matches!(std::str::from_utf8(bytes), Err(error) if error.valid_up_to() == 0 && error.error_len().is_none())
}
