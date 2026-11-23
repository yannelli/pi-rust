//! UTF-8 decoding of a stream in pieces that matches decoding all of it at once (WHATWG `TextDecoder`): U+FFFD per
//! maximal invalid subpart, and an incomplete trailing sequence waits for the next piece or the end of the stream.

// pi-env-daemon compiles this file through `#[path]` without the rest of pi-durable: use only std paths here.

/// Whether decoding the whole input drops its first three bytes as a byte-order mark.
pub fn starts_with_bom(first_bytes: &[u8]) -> bool {
    first_bytes.starts_with(&[0xef, 0xbb, 0xbf])
}

/// Decodes one stream chunk by chunk exactly like decoding all of it at once.
#[derive(Clone, Debug)]
pub struct StreamDecoder {
    /// The first bytes of a character that the next chunk can complete.
    pending: [u8; 3],
    pending_len: usize,
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
        Self { pending: [0; 3], pending_len: 0, strip_bom: true }
    }

    /// A decoder of a byte range (upstream `rangeDecoder()`): every U+FEFF is kept. Callers that start at the beginning
    /// of a file skip a leading mark themselves.
    pub fn range() -> Self {
        Self { pending: [0; 3], pending_len: 0, strip_bom: false }
    }

    /// Text for `bytes`, holding back an incomplete character.
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        let (first, complete) = self.split(bytes);
        let mut text = String::with_capacity(first.map_or(0, char::len_utf8) + complete.len());
        text.extend(first);
        push_lossy(&mut text, complete);
        text
    }

    /// `decode` for an owned chunk: text that needs no change reuses the chunk's buffer.
    pub fn decode_vec(&mut self, mut bytes: Vec<u8>) -> String {
        if self.pending_len > 0 {
            return self.decode(&bytes);
        }
        let end = complete_prefix(&bytes);
        self.hold(&bytes[end..]);
        bytes.truncate(end);
        if self.strip_bom && end > 0 {
            self.strip_bom = false;
            if starts_with_bom(&bytes) {
                bytes.drain(..3);
            }
        }
        String::from_utf8(bytes).unwrap_or_else(|error| {
            let mut text = String::with_capacity(error.as_bytes().len());
            push_replaced(&mut text, error.as_bytes());
            text
        })
    }

    /// Counts the bytes that decode would emit without allocating the decoded text.
    pub(crate) fn decoded_len(&mut self, bytes: &[u8]) -> usize {
        if self.pending_len == 0 && bytes.is_ascii() {
            self.strip_bom &= bytes.is_empty();
            return bytes.len();
        }
        let (first, complete) = self.split(bytes);
        first.map_or(0, char::len_utf8) + lossy_len(complete)
    }

    /// Counts what finish would emit without consuming the pending character.
    pub(crate) fn finish_len(&self) -> usize {
        if self.pending_len == 0 { 0 } else { char::REPLACEMENT_CHARACTER.len_utf8() }
    }

    /// The end of the stream: an incomplete character still held back becomes U+FFFD.
    pub fn finish(&mut self) -> String {
        if self.pending_len == 0 {
            return String::new();
        }
        self.pending_len = 0;
        self.strip_bom = false;
        char::REPLACEMENT_CHARACTER.to_string()
    }

    /// The character that ends the held-back bytes, if `bytes` ends it, and the complete bytes after it.
    fn split<'a>(&mut self, bytes: &'a [u8]) -> (Option<char>, &'a [u8]) {
        let mut first = None;
        let mut rest = bytes;
        if self.pending_len > 0 && !bytes.is_empty() {
            let Some((character, used)) = self.end_pending(bytes) else {
                return (None, &[]);
            };
            first = Some(character);
            rest = &bytes[used..];
        }
        // Nothing is held here: either nothing was, or `end_pending` ended it.
        let (mut complete, tail) = rest.split_at(complete_prefix(rest));
        if !tail.is_empty() {
            self.hold(tail);
        }
        if self.strip_bom && (first.is_some() || !complete.is_empty()) {
            self.strip_bom = false;
            if first == Some('\u{feff}') {
                first = None;
            } else if first.is_none() && starts_with_bom(complete) {
                complete = &complete[3..];
            }
        }
        (first, complete)
    }

    /// Completes the held-back character in a stack buffer. Returns it, or U+FFFD if a byte cannot continue it, with
    /// the count of `bytes` used; holds all of `bytes` if they end too soon.
    fn end_pending(&mut self, bytes: &[u8]) -> Option<(char, usize)> {
        let held = self.pending_len;
        let mut buffer = [0; 4];
        buffer[..held].copy_from_slice(&self.pending[..held]);
        for (index, byte) in bytes.iter().take(buffer.len() - held).enumerate() {
            buffer[held + index] = *byte;
            let ended = match std::str::from_utf8(&buffer[..=held + index]) {
                Ok(text) => text.chars().next().map(|character| (character, index + 1)),
                // The byte that cannot continue the character is decoded again as the start of the rest.
                Err(error) => error.error_len().map(|_| (char::REPLACEMENT_CHARACTER, index)),
            };
            if ended.is_some() {
                self.pending_len = 0;
                return ended;
            }
        }
        self.hold(&buffer[..held + bytes.len()]);
        None
    }

    fn hold(&mut self, bytes: &[u8]) {
        self.pending[..bytes.len()].copy_from_slice(bytes);
        self.pending_len = bytes.len();
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

fn push_lossy(text: &mut String, bytes: &[u8]) {
    match std::str::from_utf8(bytes) {
        Ok(valid) => text.push_str(valid),
        Err(_) => push_replaced(text, bytes),
    }
}

fn push_replaced(text: &mut String, bytes: &[u8]) {
    for chunk in bytes.utf8_chunks() {
        text.push_str(chunk.valid());
        if !chunk.invalid().is_empty() {
            text.push(char::REPLACEMENT_CHARACTER);
        }
    }
}

fn lossy_len(bytes: &[u8]) -> usize {
    if bytes.is_ascii() {
        return bytes.len();
    }
    bytes
        .utf8_chunks()
        .map(|chunk| {
            let replacement = if chunk.invalid().is_empty() { 0 } else { char::REPLACEMENT_CHARACTER.len_utf8() };
            chunk.valid().len() + replacement
        })
        .sum()
}
