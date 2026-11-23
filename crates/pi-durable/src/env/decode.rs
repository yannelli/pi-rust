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
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    /// A decoder of a whole stream: a leading byte-order mark is dropped, like `new TextDecoder().decode(bytes)`.
    pub fn new() -> Self {
        Self { pending: Vec::new(), strip_bom: true }
    }

    /// A decoder of a byte range (upstream `rangeDecoder()`): every U+FEFF is kept. Callers that start at the beginning
    /// of a file skip a leading mark themselves.
    pub fn range() -> Self {
        Self { pending: Vec::new(), strip_bom: false }
    }

    /// Text for `bytes`, holding back an incomplete character.
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        self.with_complete_bytes(bytes, |complete| match std::str::from_utf8(complete) {
            Ok(text) => text.to_owned(),
            Err(_) => String::from_utf8_lossy(complete).into_owned(),
        })
    }

    /// Counts the bytes that decode would emit without allocating the decoded text.
    pub(crate) fn decoded_len(&mut self, bytes: &[u8]) -> usize {
        self.with_complete_bytes(bytes, |complete| {
            if complete.is_ascii() {
                return complete.len();
            }
            complete
                .utf8_chunks()
                .map(|chunk| {
                    let replacement =
                        if chunk.invalid().is_empty() { 0 } else { char::REPLACEMENT_CHARACTER.len_utf8() };
                    chunk.valid().len() + replacement
                })
                .sum()
        })
    }

    /// Counts what finish would emit without consuming the pending character.
    pub(crate) fn finish_len(&self) -> usize {
        if self.pending.is_empty() { 0 } else { char::REPLACEMENT_CHARACTER.len_utf8() }
    }

    /// The end of the stream: an incomplete character still held back becomes U+FFFD.
    pub fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        self.pending.clear();
        self.strip_bom = false;
        char::REPLACEMENT_CHARACTER.to_string()
    }

    fn with_complete_bytes<T>(&mut self, bytes: &[u8], read: impl FnOnce(&[u8]) -> T) -> T {
        if bytes.is_empty() {
            return read(&[]);
        }
        let mut joined;
        let bytes = if self.pending.is_empty() {
            bytes
        } else {
            joined = std::mem::take(&mut self.pending);
            joined.extend_from_slice(bytes);
            &joined
        };
        let (mut complete, tail) = bytes.split_at(complete_prefix(bytes));
        self.pending.extend_from_slice(tail);
        if self.strip_bom && !complete.is_empty() {
            self.strip_bom = false;
            if starts_with_bom(complete) {
                complete = &complete[3..];
            }
        }
        read(complete)
    }
}

/// An unfinished UTF-8 character can occupy only the last one to three bytes.
fn complete_prefix(bytes: &[u8]) -> usize {
    let tail = &bytes[bytes.len().saturating_sub(3)..];
    if let Some(start) = tail.iter().rposition(|b| b & 0xc0 != 0x80) {
        let suffix = &tail[start..];
        if matches!(std::str::from_utf8(suffix), Err(error) if error.error_len().is_none()) {
            return bytes.len() - suffix.len();
        }
    }
    bytes.len()
}
