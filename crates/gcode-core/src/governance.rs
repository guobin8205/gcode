use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gcode_protocol::ServerNotification;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// 仅允许本次调用
    AllowOnce,
    /// 本会话内自动放行该工具
    AllowSession,
    /// 拒绝本次调用
    Deny,
}

/// 评估单次工具调用是否直接放行、需人工确认或直接拒绝（支持会话级动态白名单）。
#[derive(Debug, Default)]
pub struct PermissionPolicy {
    session_allowed: Mutex<HashSet<String>>,
}

impl PermissionPolicy {
    pub fn evaluate(&self, tool_name: &str) -> PermissionDecision {
        if let Ok(allowed) = self.session_allowed.lock()
            && allowed.contains(tool_name)
        {
            return PermissionDecision::Allow;
        }
        match tool_name {
            "read_file" | "grep" | "glob" | "todo" => PermissionDecision::Allow,
            "edit_file" | "write_file" | "shell" | "spawn_agent" => PermissionDecision::Ask,
            _ => PermissionDecision::Ask,
        }
    }

    /// 将指定工具加入当前会话的自动放行白名单。
    pub fn allow_for_session(&self, tool_name: &str) {
        if let Ok(mut allowed) = self.session_allowed.lock() {
            allowed.insert(tool_name.to_string());
        }
    }
}

/// 无头审批回调接口，由 TUI、Web UI 或自动化策略实现。
#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn request_approval(
        &self,
        call_id: &str,
        tool_name: &str,
        summary: &str,
    ) -> ApprovalOutcome;
}

/// 自动放行所有 `Ask` 级别工具（适用于测试或 `--yolo` 自动模式）。
#[derive(Debug, Default)]
pub struct AutoApproveHandler;

#[async_trait]
impl ApprovalHandler for AutoApproveHandler {
    async fn request_approval(
        &self,
        _call_id: &str,
        _tool_name: &str,
        _summary: &str,
    ) -> ApprovalOutcome {
        ApprovalOutcome::AllowOnce
    }
}

/// 基于事件流与 oneshot 通道的无头审批协调器，供 TUI / Web UI 异步响应。
pub struct ChannelApprovalHandler {
    tx: mpsc::UnboundedSender<ServerNotification>,
    pending: Mutex<HashMap<String, oneshot::Sender<ApprovalOutcome>>>,
}

impl ChannelApprovalHandler {
    pub fn new(tx: mpsc::UnboundedSender<ServerNotification>) -> Arc<Self> {
        Arc::new(Self {
            tx,
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// 由 UI 在用户按下确认/会话放行/拒绝后调用，唤醒等待中的工具执行协程。
    pub fn resolve(&self, call_id: &str, outcome: ApprovalOutcome) {
        if let Ok(mut map) = self.pending.lock()
            && let Some(sender) = map.remove(call_id)
        {
            let _ = sender.send(outcome);
        }
    }
}

#[async_trait]
impl ApprovalHandler for ChannelApprovalHandler {
    async fn request_approval(
        &self,
        call_id: &str,
        tool_name: &str,
        summary: &str,
    ) -> ApprovalOutcome {
        let (resp_tx, resp_rx) = oneshot::channel();
        if let Ok(mut map) = self.pending.lock() {
            map.insert(call_id.to_string(), resp_tx);
        } else {
            return ApprovalOutcome::Deny;
        }

        if self
            .tx
            .send(ServerNotification::ApprovalRequired {
                call_id: call_id.to_string(),
                tool_name: tool_name.to_string(),
                summary: summary.to_string(),
            })
            .is_err()
        {
            return ApprovalOutcome::Deny;
        }

        resp_rx.await.unwrap_or(ApprovalOutcome::Deny)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_allowlist_promotes_ask_to_allow() {
        let policy = PermissionPolicy::default();
        assert_eq!(policy.evaluate("shell"), PermissionDecision::Ask);
        policy.allow_for_session("shell");
        assert_eq!(policy.evaluate("shell"), PermissionDecision::Allow);
    }
}
