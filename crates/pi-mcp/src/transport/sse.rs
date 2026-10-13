//! Incremental Server-Sent Events parser.

use crate::error::McpError;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SseItem {
    Event(SseEvent),
    /// Every `id` field, including those of events without data, like resumption priming events.
    Id(String),
    /// Every valid `retry` field, in milliseconds.
    Retry(u64),
}

pub struct SseParser {
    max_event_bytes: usize,
    buffered: Vec<u8>,
    event_name: Option<String>,
    event_id: Option<String>,
    data_lines: Vec<String>,
    /// Bytes of the pending event's data with the joining newlines, so endless `data:` lines without
    /// a blank line cannot grow without bound.
    data_bytes: usize,
}

impl SseParser {
    pub fn new(max_event_bytes: usize) -> Self {
        Self {
            max_event_bytes,
            buffered: Vec::new(),
            event_name: None,
            event_id: None,
            data_lines: Vec::new(),
            data_bytes: 0,
        }
    }

    fn too_large(&self) -> McpError {
        McpError::other(format!("MCP SSE event exceeds {} bytes", self.max_event_bytes))
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseItem>, McpError> {
        self.buffered.extend_from_slice(chunk);
        let mut items = Vec::new();
        while let Some(newline) = self.buffered.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffered.drain(..=newline).collect();
            self.process_line(&String::from_utf8_lossy(&line[..line.len() - 1]), &mut items)?;
        }
        if self.buffered.len() > self.max_event_bytes {
            return Err(self.too_large());
        }
        Ok(items)
    }

    /// Process what is left once the stream ended.
    pub fn finish(&mut self) -> Result<Vec<SseItem>, McpError> {
        let mut items = Vec::new();
        if !self.buffered.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buffered)).into_owned();
            self.process_line(&rest, &mut items)?;
        }
        self.dispatch(&mut items);
        Ok(items)
    }

    fn dispatch(&mut self, items: &mut Vec<SseItem>) {
        let name = self.event_name.take();
        let id = self.event_id.take();
        if self.data_lines.is_empty() {
            return;
        }
        let data = std::mem::take(&mut self.data_lines).join("\n");
        self.data_bytes = 0;
        items.push(SseItem::Event(SseEvent {
            event: name.filter(|name| !name.is_empty()),
            data,
            id: id.filter(|id| !id.is_empty()),
        }));
    }

    fn process_line(&mut self, raw: &str, items: &mut Vec<SseItem>) -> Result<(), McpError> {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            self.dispatch(items);
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = match line.find(':') {
            Some(colon) => (&line[..colon], &line[colon + 1..]),
            None => (line, ""),
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                self.data_bytes += value.len() + usize::from(!self.data_lines.is_empty());
                if self.data_bytes > self.max_event_bytes {
                    return Err(self.too_large());
                }
                self.data_lines.push(value.to_string());
            }
            "event" => self.event_name = Some(value.to_string()),
            "id" if !value.contains('\0') => {
                self.event_id = Some(value.to_string());
                items.push(SseItem::Id(value.to_string()));
            }
            "retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                if let Ok(delay) = value.parse() {
                    items.push(SseItem::Retry(delay));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chunked_crlf_events_comments_ids_and_multiline_data() {
        let mut parser = SseParser::new(1024);
        let mut items = parser.push(b": keepalive\r\nid: 7\r\ndata: {\"one\":\r\n").unwrap();
        items.extend(parser.push(b"data: 1}\r\n\r\n").unwrap());
        items.extend(parser.finish().unwrap());
        let event = SseEvent { event: None, data: "{\"one\":\n1}".into(), id: Some("7".into()) };
        assert_eq!(items, vec![SseItem::Id("7".into()), SseItem::Event(event)]);
    }

    #[test]
    fn rejects_events_whose_data_lines_exceed_the_limit_without_a_blank_line() {
        let mut parser = SseParser::new(256);
        let mut sent = 0;
        let error = loop {
            sent += 1;
            if let Err(error) = parser.push(b"data: xxxxxxxxxxxxxxxx\n") {
                break error;
            }
            assert!(sent < 1000);
        };
        assert_eq!(error.to_string(), "MCP SSE event exceeds 256 bytes");
        assert!(sent < 100);
    }

    #[test]
    fn reports_retry_hints_and_priming_ids() {
        let mut parser = SseParser::new(1024);
        let items = parser.push(b"id: 1\nretry: 5\ndata:\n\n").unwrap();
        let event = SseEvent { event: None, data: String::new(), id: Some("1".into()) };
        assert_eq!(items, vec![SseItem::Id("1".into()), SseItem::Retry(5), SseItem::Event(event)]);
    }
}
