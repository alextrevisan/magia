//! Minimal incremental Server-Sent Events parser.

#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

/// One SSE event: optional `event:` name and the joined `data:` lines.
#[derive(Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((end, sep)) = find_event_end(&self.buf) {
            let raw: Vec<u8> = self.buf.drain(..end + sep).take(end).collect();
            let text = String::from_utf8_lossy(&raw);
            let mut event = None;
            let mut data: Vec<&str> = Vec::new();
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("data:") {
                    data.push(v.strip_prefix(' ').unwrap_or(v));
                } else if let Some(v) = line.strip_prefix("event:") {
                    event = Some(v.trim().to_owned());
                }
            }
            if !data.is_empty() {
                out.push(SseEvent {
                    event,
                    data: data.join("\n"),
                });
            }
        }
        out
    }
}

/// Find a blank-line event terminator; returns (index of terminator, its length).
fn find_event_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buf.len() {
        if buf[i..].starts_with(b"\n\n") {
            return Some((i, 2));
        }
        if buf[i..].starts_with(b"\r\n\r\n") {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_across_chunks() {
        let mut p = SseParser::default();
        assert!(p.push(b"data: {\"a\":").is_empty());
        let ev = p.push(b"1}\n\ndata: [DONE]\n\n");
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].data, "{\"a\":1}");
        assert_eq!(ev[1].data, "[DONE]");
    }

    #[test]
    fn named_events_and_crlf() {
        let mut p = SseParser::default();
        let ev = p.push(b"event: message_start\r\ndata: x\r\n\r\n");
        assert_eq!(ev[0].event.as_deref(), Some("message_start"));
        assert_eq!(ev[0].data, "x");
    }

    #[test]
    fn ignores_comments() {
        let mut p = SseParser::default();
        let ev = p.push(b": keep-alive\n\ndata: y\n\n");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].data, "y");
    }
}
