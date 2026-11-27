use std::collections::BTreeMap;

use async_trait::async_trait;
use futures::StreamExt;
use gcode_protocol::{Message, Role, TokenUsage, ToolCall};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::config::ProviderConfig;
use crate::provider::{
    ModelError, ModelProvider, ModelRequest, ModelStream, StreamChunk, format_error_detail,
    parse_retry_after_secs,
};
use crate::sse::SseParser;

/// OpenAI 兼容协议适配器，支持 DeepSeek `reasoning_content`、流式工具分片与缓存统计。
pub struct OpenAiCompatibleProvider {
    client: Client,
    config: ProviderConfig,
}

impl OpenAiCompatibleProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            client: Client::new(),
            config,
        }
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }

    pub fn build_payload(&self, request: &ModelRequest) -> Value {
        let messages: Vec<Value> = request.messages.iter().map(serialize_openai_message).collect();
        let mut body = json!({
            "model": request.model_id,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });

        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = Value::Array(tools);
        }

        body
    }
}

fn serialize_openai_message(msg: &Message) -> Value {
    let role = match msg.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };

    let mut obj = json!({
        "role": role,
        "content": msg.content,
    });

    if let Some(reasoning) = &msg.reasoning_content {
        obj["reasoning_content"] = Value::String(reasoning.clone());
    }
    if let Some(call_id) = &msg.tool_call_id {
        obj["tool_call_id"] = Value::String(call_id.clone());
    }
    if !msg.tool_calls.is_empty() {
        let calls: Vec<Value> = msg
            .tool_calls
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "type": "function",
                    "function": {
                        "name": c.name,
                        "arguments": serde_json::to_string(&c.arguments).unwrap_or_else(|_| "{}".to_string()),
                    }
                })
            })
            .collect();
        obj["tool_calls"] = Value::Array(calls);
    }

    obj
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments_buf: String,
}

#[derive(Default)]
pub struct OpenAiStreamAccumulator {
    pending_tools: BTreeMap<usize, PartialToolCall>,
    latest_usage: TokenUsage,
}

impl OpenAiStreamAccumulator {
    pub fn feed_data(&mut self, data: &str) -> Result<Vec<StreamChunk>, ModelError> {
        if data == "[DONE]" {
            return Ok(self.flush());
        }

        let chunk: OpenAiChunk =
            serde_json::from_str(data).map_err(|e| ModelError::Protocol(format!("SSE JSON 解析失败: {e}")))?;
        let mut out = Vec::new();

        if let Some(usage) = chunk.usage {
            let cache_hit = usage
                .prompt_cache_hit_tokens
                .or_else(|| usage.prompt_tokens_details.as_ref().and_then(|d| d.cached_tokens))
                .unwrap_or(0);
            let cache_miss = usage
                .prompt_cache_miss_tokens
                .unwrap_or_else(|| usage.prompt_tokens.saturating_sub(cache_hit));
            self.latest_usage = TokenUsage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                cache_hit_tokens: cache_hit,
                cache_miss_tokens: cache_miss,
            };
        }

        for choice in chunk.choices {
            if let Some(delta) = choice.delta {
                if let Some(reasoning) = delta.reasoning_content.or(delta.reasoning)
                    && !reasoning.is_empty()
                {
                    out.push(StreamChunk::ReasoningDelta(reasoning));
                }
                if let Some(content) = delta.content
                    && !content.is_empty()
                {
                    out.push(StreamChunk::TextDelta(content));
                }
                for tc in delta.tool_calls {
                    let entry = self.pending_tools.entry(tc.index.unwrap_or(0)).or_default();
                    if let Some(id) = tc.id {
                        entry.id = id;
                    }
                    if let Some(func) = tc.function {
                        if let Some(name) = func.name {
                            entry.name.push_str(&name);
                        }
                        if let Some(args) = func.arguments {
                            entry.arguments_buf.push_str(&args);
                        }
                    }
                }
            }

            if choice.finish_reason.as_deref() == Some("tool_calls") {
                out.extend(self.drain_tools());
            }
        }

        Ok(out)
    }

    pub fn flush(&mut self) -> Vec<StreamChunk> {
        let mut out = self.drain_tools();
        out.push(StreamChunk::Done(self.latest_usage.clone()));
        out
    }

    fn drain_tools(&mut self) -> Vec<StreamChunk> {
        let pending = std::mem::take(&mut self.pending_tools);
        pending
            .into_values()
            .filter(|t| !t.name.is_empty())
            .map(|t| {
                let arguments = if t.arguments_buf.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&t.arguments_buf).unwrap_or_else(|_| Value::String(t.arguments_buf))
                };
                StreamChunk::ToolCallCompleted(ToolCall {
                    id: t.id,
                    name: t.name,
                    arguments,
                })
            })
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiChunk {
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    delta: Option<OpenAiDelta>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAiToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct OpenAiToolCallDelta {
    index: Option<usize>,
    id: Option<String>,
    function: Option<OpenAiFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct OpenAiFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    prompt_cache_hit_tokens: Option<u64>,
    prompt_cache_miss_tokens: Option<u64>,
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<u64>,
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
    async fn stream_chat(&self, request: ModelRequest) -> Result<ModelStream, ModelError> {
        let payload = self.build_payload(&request);
        let resp = self
            .client
            .post(self.endpoint())
            .bearer_auth(&self.config.api_key)
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
            let mut acc = OpenAiStreamAccumulator::default();
            let mut done_emitted = false;

            while let Some(item) = byte_stream.next().await {
                match item {
                    Ok(bytes) => {
                        for sse in parser.push(&bytes) {
                            match acc.feed_data(&sse.data) {
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
                for sse in parser.finish() {
                    if let Ok(chunks) = acc.feed_data(&sse.data) {
                        for c in chunks {
                            if matches!(c, StreamChunk::Done(_)) {
                                done_emitted = true;
                            }
                            let _ = tx.send(Ok(c));
                        }
                    }
                }
                if !done_emitted {
                    for c in acc.flush() {
                        let _ = tx.send(Ok(c));
                    }
                }
            }
        });

        Ok(Box::pin(tokio_stream_from_rx(rx)))
    }
}

pub(crate) fn tokio_stream_from_rx(
    rx: mpsc::UnboundedReceiver<Result<StreamChunk, ModelError>>,
) -> impl futures::Stream<Item = Result<StreamChunk, ModelError>> {
    futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_deepseek_reasoning_tool_calls_and_cache_usage() {
        let mut acc = OpenAiStreamAccumulator::default();
        let frames = [
            r#"{"choices":[{"delta":{"reasoning_content":"先查文件"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"src/main.rs\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":50,"prompt_cache_hit_tokens":960}}"#,
        ];

        let mut chunks = Vec::new();
        for f in frames {
            chunks.extend(acc.feed_data(f).unwrap());
        }
        chunks.extend(acc.feed_data("[DONE]").unwrap());

        assert!(matches!(&chunks[0], StreamChunk::ReasoningDelta(s) if s == "先查文件"));
        assert!(matches!(
            &chunks[1],
            StreamChunk::ToolCallCompleted(tc) if tc.id == "call_1" && tc.name == "read_file" && tc.arguments["path"] == "src/main.rs"
        ));
        assert!(matches!(
            &chunks[2],
            StreamChunk::Done(u) if u.cache_hit_tokens == 960 && u.cache_miss_tokens == 40
        ));
    }
}
