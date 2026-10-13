//! Length-prefixed framing: a four-byte unsigned big-endian payload length, then the payload.

const FRAME_HEADER_LENGTH: usize = 4;
const PAYLOAD_BLOCK_SIZE: usize = 64 * 1024;

/// Default upper bound for one framed CBOR payload.
pub const DEFAULT_MAX_FRAME_LENGTH: u32 = 16 * 1024 * 1024;

/// Options for [`FrameDecoder`] and the message codecs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameDecoderOptions {
    /// Maximum payload bytes of one frame. Defaults to [`DEFAULT_MAX_FRAME_LENGTH`].
    pub max_frame_length: Option<u32>,
}

impl FrameDecoderOptions {
    pub fn with_max_frame_length(max_frame_length: u32) -> Self {
        Self { max_frame_length: Some(max_frame_length) }
    }

    pub(crate) fn max_frame_length(&self) -> u32 {
        self.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH)
    }
}

/// Invalid, oversized, or truncated framing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FrameError(pub String);

/// Prefix a payload with its unsigned 32-bit big-endian byte length.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let length = u32::try_from(payload.len())
        .map_err(|_| FrameError("Frame payload exceeds the unsigned 32-bit length limit".into()))?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_LENGTH + payload.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DecoderState {
    Open,
    Ended,
    Failed,
}

/// Incrementally splits arbitrary byte chunks into length-prefixed payloads.
pub struct FrameDecoder {
    header: [u8; FRAME_HEADER_LENGTH],
    header_length: usize,
    max_frame_length: u32,
    payload: Vec<u8>,
    expected: Option<usize>,
    state: DecoderState,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new(FrameDecoderOptions::default())
    }
}

impl FrameDecoder {
    pub fn new(options: FrameDecoderOptions) -> Self {
        Self {
            header: [0; FRAME_HEADER_LENGTH],
            header_length: 0,
            max_frame_length: options.max_frame_length(),
            payload: Vec::new(),
            expected: None,
            state: DecoderState::Open,
        }
    }

    fn check_open(&self) -> Result<(), FrameError> {
        match self.state {
            DecoderState::Open => Ok(()),
            DecoderState::Ended => Err(FrameError("Frame decoder has ended".into())),
            DecoderState::Failed => Err(FrameError("Frame decoder has failed".into())),
        }
    }

    /// Return every payload completed by `chunk`. Payloads never alias the input.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        self.check_open()?;
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset < chunk.len() {
            let expected = match self.expected {
                Some(expected) => expected,
                None => {
                    let count = (FRAME_HEADER_LENGTH - self.header_length).min(chunk.len() - offset);
                    self.header[self.header_length..self.header_length + count]
                        .copy_from_slice(&chunk[offset..offset + count]);
                    self.header_length += count;
                    offset += count;
                    if self.header_length < FRAME_HEADER_LENGTH {
                        continue;
                    }
                    self.header_length = 0;
                    let length = u32::from_be_bytes(self.header);
                    if length > self.max_frame_length {
                        return Err(self.fail(format!(
                            "Frame length {length} exceeds configured limit of {}",
                            self.max_frame_length
                        )));
                    }
                    if length == 0 {
                        frames.push(Vec::new());
                        continue;
                    }
                    let length = length as usize;
                    self.expected = Some(length);
                    self.payload = Vec::with_capacity(length.min(PAYLOAD_BLOCK_SIZE));
                    length
                }
            };
            let count = (expected - self.payload.len()).min(chunk.len() - offset);
            self.payload.extend_from_slice(&chunk[offset..offset + count]);
            offset += count;
            if self.payload.len() == expected {
                frames.push(std::mem::take(&mut self.payload));
                self.expected = None;
            }
        }
        Ok(frames)
    }

    /// Finish the stream, failing when a frame is incomplete.
    pub fn end(&mut self) -> Result<(), FrameError> {
        self.check_open()?;
        if self.header_length != 0 || self.expected.is_some() {
            return Err(self.fail("Truncated frame at end of stream".into()));
        }
        self.state = DecoderState::Ended;
        Ok(())
    }

    fn fail(&mut self, message: String) -> FrameError {
        self.state = DecoderState::Failed;
        self.header_length = 0;
        self.payload = Vec::new();
        self.expected = None;
        FrameError(message)
    }
}
