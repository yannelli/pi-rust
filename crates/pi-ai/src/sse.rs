//! Server-sent events framing over a byte stream.

use futures::{Stream, StreamExt};

/// Largest accepted event, to bound memory on a misbehaving server.
const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Split a byte stream into SSE events. Bytes are buffered until a full line
/// arrives, so multi-byte UTF-8 characters split across chunks decode correctly.
pub fn sse_events<S, B, E>(body: S) -> impl Stream<Item = Result<SseEvent, String>>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    futures::stream::unfold(
        (body, Vec::<u8>::new(), SseEvent::default(), Vec::<String>::new(), false),
        |(mut body, mut buffer, mut current, mut data, mut done)| async move {
            loop {
                if let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
                    let mut line: Vec<u8> = buffer.drain(..=position).collect();
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    let line = String::from_utf8_lossy(&line).into_owned();
                    if line.is_empty() {
                        if !data.is_empty() || current.event.is_some() {
                            current.data = data.join("\n");
                            data.clear();
                            let event = std::mem::take(&mut current);
                            return Some((Ok(event), (body, buffer, current, data, done)));
                        }
                        continue;
                    }
                    apply_line(&line, &mut current, &mut data);
                    continue;
                }
                if done {
                    if !buffer.is_empty() {
                        let line = String::from_utf8_lossy(&std::mem::take(&mut buffer)).into_owned();
                        apply_line(line.trim_end_matches('\r'), &mut current, &mut data);
                    }
                    if !data.is_empty() {
                        current.data = data.join("\n");
                        data.clear();
                        let event = std::mem::take(&mut current);
                        return Some((Ok(event), (body, buffer, current, data, done)));
                    }
                    return None;
                }
                match body.next().await {
                    Some(Ok(chunk)) => {
                        buffer.extend_from_slice(chunk.as_ref());
                        if buffer.len() > MAX_EVENT_BYTES {
                            done = true;
                            buffer.clear();
                            data.clear();
                            return Some((Err("SSE event exceeds 16 MiB".to_string()), (body, buffer, current, data, done)));
                        }
                    }
                    Some(Err(error)) => {
                        done = true;
                        buffer.clear();
                        data.clear();
                        return Some((Err(error.to_string()), (body, buffer, current, data, done)));
                    }
                    None => done = true,
                }
            }
        },
    )
}

fn apply_line(line: &str, current: &mut SseEvent, data: &mut Vec<String>) {
    if line.starts_with(':') {
        return;
    }
    let (field, value) = match line.split_once(':') {
        Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
        None => (line, ""),
    };
    match field {
        "data" => data.push(value.to_string()),
        "event" => current.event = Some(value.to_string()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    #[tokio::test]
    async fn frames_events_across_chunk_boundaries() {
        let bytes = "event: a\ndata: {\"x\":\"é\"}\r\n\r\n: comment\ndata: one\ndata: two\n\ndata: tail".as_bytes();
        let chunks: Vec<Result<Vec<u8>, String>> = bytes.chunks(3).map(|chunk| Ok(chunk.to_vec())).collect();
        let events: Vec<_> = sse_events(futures::stream::iter(chunks)).collect().await;
        let events: Vec<SseEvent> = events.into_iter().map(Result::unwrap).collect();
        assert_eq!(events[0], SseEvent { event: Some("a".into()), data: "{\"x\":\"é\"}".into() });
        assert_eq!(events[1].data, "one\ntwo");
        assert_eq!(events[2].data, "tail");
        assert_eq!(events.len(), 3);
    }
}
