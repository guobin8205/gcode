use std::collections::BTreeMap;

use async_trait::async_trait;
use futures::StreamExt;
use gcode_protocol::{Role, TokenUsage, ToolCall};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::config::ProviderConfig;
use crate::openai::tokio_stream_from_rx;
use crate::provider::{
    ModelError, ModelProvider, ModelRequest, ModelStream, StreamChunk, format_error_detail,
    parse_retry_after_secs,
};
use crate::sse::SseParser;

/// Anthropic Messages 协议适配器，兼容 DeepSeek / GLM / Kimi 的 `/anthropic` 兼容端点。
pub struct AnthropicProvider {
    client: Client,
    config: ProviderConfig,
}

impl AnthropicProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            client: Client::new(),
            config,
        }
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if base.ends_with("/v1/messages") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }

    pub fn build_payload(&self, request: &ModelRequest) -> Value {
        let mut system_parts = Vec::new();
        let mut anthropic_messages: Vec<Value> = Vec::new();
        let mut pending_tool_results: Vec<Value> = Vec::new();

        let flush_tool_results = |pending: &mut Vec<Value>, out: &mut Vec<Value>| {
            if !pending.is_empty() {
                out.push(json!({
                    "role": "user",
                    "content": std::mem::take(pending),
                }));
            }
        };

        for msg in &request.messages {
            if msg.role == Role::Tool {
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": msg.tool_call_id.clone().unwrap_or_default(),
                    "content": msg.content,
                }));
                continue;
            }

            flush_tool_results(&mut pending_tool_results, &mut anthropic_messages);

            match msg.role {
                Role::System => {
                    if !msg.content.is_empty() {
                        system_parts.push(msg.content.clone());
                    }
                }
                Role::User => {
                    anthropic_messages.push(json!({
                        "role": "user",
                        "content": msg.content,
                    }));
                }
                Role::Assistant => {
                    let mut blocks = Vec::new();
                    if let Some(thinking) = &msg.reasoning_content
                        && !thinking.is_empty()
                    {
                        blocks.push(json!({ "type": "thinking", "thinking": thinking }));
                    }
                    if !msg.content.is_empty() {
                        blocks.push(json!({ "type": "text", "text": msg.content }));
                    }
                    for tc in &msg.tool_calls {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name,
                            "input": tc.arguments,
                        }));
                    }
                    anthropic_messages.push(json!({
                        "role": "assistant",
                        "content": blocks,
                    }));
                }
                Role::Tool => unreachable!(),
            }
        }
        flush_tool_results(&mut pending_tool_results, &mut anthropic_messages);

        let mut body = json!({
            "model": request.model_id,
            "max_tokens": 32768,
            "stream": true,
            "messages": anthropic_messages,
        });

        if !system_parts.is_empty() {
            body["system"] = json!([{
                "type": "text",
                "text": system_parts.join("\n\n"),
                "cache_control": { "type": "ephemeral" }
            }]);
        }

        if !request.tools.is_empty() {
            let last_idx = request.tools.len() - 1;
            let tools: Vec<Value> = request
                .tools
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let mut obj = json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    });
                    if i == last_idx {
                        obj["cache_control"] = json!({ "type": "ephemeral" });
                    }
                    obj
                })
                .collect();
            body["tools"] = Value::Array(tools);
        }

        body
    }
}

struct ActiveToolBlock {
    id: String,
    name: String,
    input_json: String,
}

#[derive(Default)]
pub struct AnthropicStreamAccumulator {
    active_tools: BTreeMap<usize, ActiveToolBlock>,
    usage: TokenUsage,
}

impl AnthropicStreamAccumulator {
    pub fn feed_event(&mut self, event_name: Option<&str>, data: &str) -> Result<Vec<StreamChunk>, ModelError> {
        let val: Value =
            serde_json::from_str(data).map_err(|e| ModelError::Protocol(format!("Anthropic SSE 解析失败: {e}")))?;
        let kind = event_name
            .or_else(|| val.get("type").and_then(Value::as_str))
            .unwrap_or_default();

        let mut out = Vec::new();
        match kind {
            "message_start" => {
                if let Some(u) = val.pointer("/message/usage") {
                    self.update_usage(u);
                }
            }
            "content_block_start" => {
                let index = val.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(block) = val.get("content_block")
                    && block.get("type").and_then(Value::as_str) == Some("tool_use")
                {
                    self.active_tools.insert(
                        index,
                        ActiveToolBlock {
                            id: block.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                            name: block.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                            input_json: String::new(),
                        },
                    );
                }
            }
            "content_block_delta" => {
                let index = val.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(delta) = val.get("delta") {
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(text) = delta.get("text").and_then(Value::as_str)
                                && !text.is_empty()
                            {
                                out.push(StreamChunk::TextDelta(text.to_string()));
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(thinking) = delta.get("thinking").and_then(Value::as_str)
                                && !thinking.is_empty()
                            {
                                out.push(StreamChunk::ReasoningDelta(thinking.to_string()));
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(partial) = delta.get("partial_json").and_then(Value::as_str)
                                && let Some(tool) = self.active_tools.get_mut(&index)
                            {
                                tool.input_json.push_str(partial);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                let index = val.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(tool) = self.active_tools.remove(&index) {
                    let arguments = if tool.input_json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&tool.input_json).unwrap_or_else(|_| Value::String(tool.input_json))
                    };
                    out.push(StreamChunk::ToolCallCompleted(ToolCall {
                        id: tool.id,
                        name: tool.name,
                        arguments,
                    }));
                }
            }
            "message_delta" => {
                if let Some(u) = val.get("usage") {
                    self.update_usage(u);
                }
            }
            "message_stop" => {
                out.push(StreamChunk::Done(self.usage.clone()));
            }
            _ => {}
        }

        Ok(out)
    }

    fn update_usage(&mut self, val: &Value) {
        if let Ok(u) = serde_json::from_value::<AnthropicUsage>(val.clone()) {
            if u.input_tokens > 0 {
                self.usage.prompt_tokens = u.input_tokens;
                self.usage.cache_miss_tokens = u.input_tokens;
            }
            if u.output_tokens > 0 {
                self.usage.completion_tokens = u.output_tokens;
            }
            if let Some(hit) = u.cache_read_input_tokens {
                self.usage.cache_hit_tokens = hit;
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    cache_read_input_tokens: Option<u64>,
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    async fn stream_chat(&self, request: ModelRequest) -> Result<ModelStream, ModelError> {
        let payload = self.build_payload(&request);
        let resp = self
            .client
            .post(self.endpoint())
            .header("x-api-key", &self.config.api_key)
            .bearer_auth(&self.config.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&payload)
            .send()
            .await
            .map_err(|e| ModelError::Transport(e.to_string()))?;

        let status = resp.status();
        if status.as_u16() == 429 {
            let retry_after_secs = parse_retry_after_secs(resp.headers());
            let body = resp.text().await.unwrap_or_default();
            return Err(ModelError::RateLimited {
                retry_after_secs,
                detail: format_error_detail(&body),
            });
        }
        if matches!(status.as_u16(), 500 | 502 | 503 | 504 | 529) {
            let body = resp.text().await.unwrap_or_default();
            return Err(ModelError::ServerOverloaded {
                status: status.as_u16(),
                detail: format_error_detail(&body),
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ModelError::Protocol(format!("HTTP {status}: {body}")));
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let mut byte_stream = resp.bytes_stream();

        tokio::spawn(async move {
            let mut parser = SseParser::default();
            let mut acc = AnthropicStreamAccumulator::default();
            let mut done_emitted = false;

            while let Some(item) = byte_stream.next().await {
                match item {
                    Ok(bytes) => {
                        for sse in parser.push(&bytes) {
                            match acc.feed_event(sse.event.as_deref(), &sse.data) {
                                Ok(chunks) => {
                                    for c in chunks {
                                        if matches!(c, StreamChunk::Done(_)) {
                                            done_emitted = true;
                                        }
                                        if tx.send(Ok(c)).is_err() {
                                            return;
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = tx.send(Err(e));
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(ModelError::Transport(e.to_string())));
                        return;
                    }
                }
            }

            if !done_emitted {
                let _ = tx.send(Ok(StreamChunk::Done(acc.usage)));
            }
        });

        Ok(Box::pin(tokio_stream_from_rx(rx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_anthropic_thinking_tool_use_and_cache() {
        let mut acc = AnthropicStreamAccumulator::default();
        let events = [
            ("message_start", r#"{"type":"message_start","message":{"usage":{"input_tokens":120,"cache_read_input_tokens":800}}}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"分析中"}}"#),
            ("content_block_start", r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"grep"}}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pattern\":\"main\"}"}}"#),
            ("content_block_stop", r#"{"type":"content_block_stop","index":1}"#),
            ("message_delta", r#"{"type":"message_delta","usage":{"output_tokens":42}}"#),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ];

        let mut chunks = Vec::new();
        for (ev, data) in events {
            chunks.extend(acc.feed_event(Some(ev), data).unwrap());
        }

        assert!(matches!(&chunks[0], StreamChunk::ReasoningDelta(s) if s == "分析中"));
        assert!(matches!(
            &chunks[1],
            StreamChunk::ToolCallCompleted(tc) if tc.id == "tu_1" && tc.name == "grep" && tc.arguments["pattern"] == "main"
        ));
        assert!(matches!(
            &chunks[2],
            StreamChunk::Done(u) if u.cache_hit_tokens == 800 && u.completion_tokens == 42
        ));
    }
}
