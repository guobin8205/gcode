use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use gcode_protocol::{Message, TokenUsage, ToolCall, ToolDefinition};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("network or transport error: {0}")]
    Transport(String),
    #[error("protocol/schema error: {0}")]
    Protocol(String),
    #[error("rate limited or quota exceeded{detail}")]
    RateLimited {
        retry_after_secs: Option<u64>,
        detail: String,
    },
    #[error("upstream server error (HTTP {status}){detail}")]
    ServerOverloaded { status: u16, detail: String },
}

impl ModelError {
    /// 判断该错误是否属于可自动重试的瞬时故障（429 限流、5xx 过载、网络抖动）。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ModelError::RateLimited { .. }
                | ModelError::ServerOverloaded { .. }
                | ModelError::Transport(_)
        )
    }

    /// 若服务端返回了 `Retry-After` 秒数，则优先采用该建议等待时长。
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            ModelError::RateLimited {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
    }
}

/// 从 HTTP 响应中提取 `Retry-After` 秒数（仅解析非负整数秒格式）。
pub(crate) fn parse_retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// 将错误响应体格式化为简洁摘要，便于在 UI 中展示具体原因。
pub(crate) fn format_error_detail(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let truncated: String = trimmed.chars().take(200).collect();
    if trimmed.chars().count() > 200 {
        format!(": {truncated}...")
    } else {
        format!(": {truncated}")
    }
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub model_id: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone)]
pub enum StreamChunk {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallCompleted(ToolCall),
    Done(TokenUsage),
}

pub type ModelStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, ModelError>> + Send>>;

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn stream_chat(&self, request: ModelRequest) -> Result<ModelStream, ModelError>;
}
