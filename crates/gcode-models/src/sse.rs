/// 增量式 Server-Sent Events (SSE) 解析器，安全处理跨 chunk 的 UTF-8 边界与 `\r\n`。
#[derive(Debug, Default)]
pub struct SseParser {
    raw_buf: Vec<u8>,
    current_event: Option<String>,
    data_lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.raw_buf.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some(pos) = self.raw_buf.iter().position(|&b| b == b'\n') {
            let mut line_bytes: Vec<u8> = self.raw_buf.drain(..=pos).collect();
            // 去除末尾 '\n' 与可选 '\r'
            if line_bytes.last() == Some(&b'\n') {
                line_bytes.pop();
            }
            if line_bytes.last() == Some(&b'\r') {
                line_bytes.pop();
            }
            let line = String::from_utf8_lossy(&line_bytes);
            self.process_line(&line, &mut events);
        }

        events
    }

    /// 流结束时刷新缓冲区中可能残留的最后一条事件。
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.raw_buf.is_empty() {
            let line_bytes = std::mem::take(&mut self.raw_buf);
            let line = String::from_utf8_lossy(&line_bytes);
            self.process_line(line.trim_end_matches('\r'), &mut events);
        }
        if !self.data_lines.is_empty() {
            events.push(SseEvent {
                event: self.current_event.take(),
                data: std::mem::take(&mut self.data_lines).join("\n"),
            });
        }
        events
    }

    fn process_line(&mut self, line: &str, events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if !self.data_lines.is_empty() {
                events.push(SseEvent {
                    event: self.current_event.take(),
                    data: std::mem::take(&mut self.data_lines).join("\n"),
                });
            } else {
                self.current_event = None;
            }
            return;
        }

        if line.starts_with(':') {
            return;
        }

        if let Some(rest) = line.strip_prefix("event:") {
            self.current_event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            self.data_lines.push(rest.trim_start().to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_utf8_and_crlf_events() {
        let mut parser = SseParser::default();
        let part1 = b"event: content_block_delta\r\ndata: {\"text\":\"\xe4\xb8";
        let part2 = b"\xad\xe6\x96\x87\"}\r\n\r\ndata: [DONE]\n\n";

        let mut out = parser.push(part1);
        assert!(out.is_empty());
        out.extend(parser.push(part2));

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event.as_deref(), Some("content_block_delta"));
        assert_eq!(out[0].data, "{\"text\":\"中文\"}");
        assert_eq!(out[1].data, "[DONE]");
    }
}
