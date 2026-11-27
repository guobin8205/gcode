use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::message::ToolCall;

/// 记录单次推理 Step 的 Token 消耗与前缀缓存命中指标。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
}

/// 持久化到 `.gcode/sessions/<id>.jsonl` 的只追加（Append-Only）事件信封。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    pub turn_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_id: Option<u64>,
    #[serde(default)]
    pub ignorable: bool,
    pub kind: EventKind,
}

/// 核心领域事件；UI、对话上下文与统计指标均是对该事件流的投影。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    SessionStarted {
        session_id: String,
        workspace: String,
        model_id: String,
    },
    TurnStarted {
        user_input: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        git_tree_hash: Option<String>,
    },
    AssistantMessage {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        usage: TokenUsage,
    },
    ToolResult {
        call_id: String,
        tool_name: String,
        output: String,
        is_error: bool,
    },
    TurnFinished {
        aborted: bool,
    },
    CompactionPerformed {
        summary: String,
        first_kept_seq: u64,
    },
    /// 对话与磁盘回滚/分叉事件：截断 `target_turn_id` 及之后的活跃轨迹，同时保留完整只追加日志。
    TurnForked {
        target_turn_id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        restored_git_tree: Option<String>,
    },
    Error {
        message: String,
    },
}
