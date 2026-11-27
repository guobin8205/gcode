use serde::{Deserialize, Serialize};

use crate::event::SessionEvent;

/// 由 TUI / Web UI 发送给 `gcode-core` 的控制指令。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCommand {
    SubmitPrompt { input: String },
    CancelTurn,
    ApproveTool { call_id: String, allowed: bool },
    ForkTurn { target_turn_id: u64 },
}

/// 由 `gcode-core` 推送给任意挂载界面的通知事件流。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerNotification {
    EventAppended { event: SessionEvent },
    StreamDelta {
        turn_id: u64,
        step_id: u64,
        text_delta: Option<String>,
        reasoning_delta: Option<String>,
    },
    ApprovalRequired {
        call_id: String,
        tool_name: String,
        summary: String,
    },
    /// 瞬时状态提示（如限流自动重试倒计时），仅用于前端展示，不持久化到事件日志。
    StatusMessage {
        message: String,
    },
    /// 会话分叉/回滚后通知前端按活跃事件流重建视图。
    SessionRebuilt {
        events: Vec<SessionEvent>,
    },
}
