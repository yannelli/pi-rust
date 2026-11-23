//! UTF-8 decoding of a stream in pieces that matches decoding all of it at once (WHATWG `TextDecoder`): U+FFFD per
//! maximal invalid subpart, and an incomplete trailing sequence waits for the next piece or the end of the stream.

// pi-env-daemon compiles this file through `#[path]` without the rest of pi-durable: use only std paths here.

/// Whether decoding the whole input drops its first three bytes as a byte-order mark.
pub fn starts_with_bom(first_bytes: &[u8]) -> bool {
    first_bytes.starts_with(&[0xef, 0xbb, 0xbf])
}

/// The number of `\n` bytes in `bytes`, counted eight at a time in a `u64` so the speed does not depend on the
/// optimizer vectorizing a loop.
pub fn count_newlines(bytes: &[u8]) -> usize {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const LOW7: u64 = 0x7f7f_7f7f_7f7f_7f7f;
    let mut total = 0;
    // Each byte lane counts to at most 255 before the lanes are summed.
    for run in bytes.chunks(255 * 8) {
        let mut words = run.chunks_exact(8);
        let mut lanes = 0u64;
        for word in &mut words {
            let other = u64::from_le_bytes(word.try_into().expect("8 bytes")) ^ (ONES * u64::from(b'\n'));
            // A lane's high bit is set when the lane is nonzero, without carries between lanes.
            let nonzero = ((other & LOW7) + LOW7) | other;
            lanes += (!nonzero >> 7) & ONES;
        }
        let pairs = (lanes & 0x00ff_00ff_00ff_00ff) + ((lanes >> 8) & 0x00ff_00ff_00ff_00ff);
        total += (pairs.wrapping_mul(0x0001_0001_0001_0001) >> 48) as usize;
        total += words.remainder().iter().filter(|byte| **byte == b'\n').count();
    }
    total
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
        self.cut_complete(&mut bytes);
        String::from_utf8(bytes).unwrap_or_else(|error| {
            let mut text = String::with_capacity(error.as_bytes().len());
            push_replaced(&mut text, error.as_bytes());
            text
        })
    }

    /// `decode_vec` as bytes: the same text, in the chunk's buffer when its complete part is valid.
    pub fn decode_bytes(&mut self, mut bytes: Vec<u8>) -> Vec<u8> {
        if self.pending_len > 0 {
            let (first, complete) = self.split(&bytes);
            let mut text = Vec::with_capacity(first.map_or(0, char::len_utf8) + complete.len());
            if let Some(first) = first {
                text.extend_from_slice(first.encode_utf8(&mut [0; 4]).as_bytes());
            }
            push_pieces(&mut text, complete);
            return text;
        }
        self.cut_complete(&mut bytes);
        if super::utf8::is_utf8(&bytes) {
            return bytes;
        }
        let mut text = Vec::with_capacity(bytes.len());
        push_pieces(&mut text, &bytes);
        text
    }

    /// Raw bytes for a reader that decodes each result on its own: the held-back bytes, then the chunk, cut before an
    /// unfinished character and without the stream's leading mark. Nothing is validated. `String::from_utf8_lossy` of
    /// the result is the text `decode` returns for the same chunk.
    pub fn pass_through(&mut self, mut bytes: Vec<u8>) -> Vec<u8> {
        if self.pending_len > 0 {
            bytes.splice(..0, self.pending[..self.pending_len].iter().copied());
            self.pending_len = 0;
        }
        self.cut_complete(&mut bytes);
        bytes
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

    /// The end of a `pass_through` stream: the held-back bytes, which decode to the U+FFFD that `finish` returns.
    pub fn finish_raw(&mut self) -> Vec<u8> {
        let held = self.pending[..self.pending_len].to_vec();
        if !held.is_empty() {
            self.pending_len = 0;
            self.strip_bom = false;
        }
        held
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

    /// Holds back an unfinished character at the end of `bytes` and drops the stream's leading mark. Nothing may be
    /// held already.
    fn cut_complete(&mut self, bytes: &mut Vec<u8>) {
        let end = complete_prefix(bytes);
        self.hold(&bytes[end..]);
        bytes.truncate(end);
        if self.strip_bom && end > 0 {
            self.strip_bom = false;
            if starts_with_bom(bytes) {
                bytes.drain(..3);
            }
        }
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

/// Invalid text is replaced piece by piece, so one bad byte sends only its piece through `utf8_chunks`.
const PIECE: usize = 4096;

/// Appends the lossy text of `bytes`. A cut before a byte that is not a continuation byte, or after three continuation
/// bytes, never falls inside a character or a maximal invalid subpart.
fn push_pieces(text: &mut Vec<u8>, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let mut end = bytes.len().min(PIECE);
        if end < bytes.len()
            && let Some(back) = (0..=3).find(|back| bytes[end - back] & 0xc0 != 0x80)
        {
            end -= back;
        }
        let (piece, rest) = bytes.split_at(end);
        if super::utf8::is_utf8(piece) {
            text.extend_from_slice(piece);
        } else {
            for chunk in piece.utf8_chunks() {
                text.extend_from_slice(chunk.valid().as_bytes());
                if !chunk.invalid().is_empty() {
                    text.extend_from_slice("\u{fffd}".as_bytes());
                }
            }
        }
        bytes = rest;
    }
}

fn lossy_len(bytes: &[u8]) -> usize {
    if super::utf8::is_utf8(bytes) {
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
