use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gcode_protocol::ServerNotification;
use serde::{Deserialize, Serialize};
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
    /// 本工作区永久记住并自动放行该工具
    AllowSession,
    /// 拒绝本次调用
    Deny,
}

/// 权限运行模式：
/// - `Auto`（默认）：只读工具与工作区文件编辑自动放行（配合 Git 快照 `/undo` 兜底），仅 `shell` / `spawn_agent` 需确认
/// - `Yolo`：所有工具全部自动放行，零打扰
/// - `Ask`：严格模式，所有文件修改与命令执行均需确认
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    #[default]
    Auto,
    Yolo,
    Ask,
}

impl PermissionMode {
    pub fn label(self) -> &'static str {
        match self {
            PermissionMode::Auto => "auto",
            PermissionMode::Yolo => "yolo",
            PermissionMode::Ask => "ask",
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            PermissionMode::Auto => PermissionMode::Yolo,
            PermissionMode::Yolo => PermissionMode::Ask,
            PermissionMode::Ask => PermissionMode::Auto,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "auto" => Some(PermissionMode::Auto),
            "yolo" => Some(PermissionMode::Yolo),
            "ask" | "strict" => Some(PermissionMode::Ask),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedPermissions {
    #[serde(default)]
    allowed_tools: Vec<String>,
}

/// 评估单次工具调用是否直接放行、需人工确认或直接拒绝（支持模式切换与工作区持久化白名单）。
#[derive(Debug, Default)]
pub struct PermissionPolicy {
    mode: Mutex<PermissionMode>,
    session_allowed: Mutex<HashSet<String>>,
    persist_path: Option<PathBuf>,
}

impl PermissionPolicy {
    /// 从 `<workspace>/.gcode/permissions.json` 加载已保存的放行白名单。
    pub fn load_for_workspace(workspace_root: &Path) -> Self {
        let persist_path = workspace_root.join(".gcode").join("permissions.json");
        let mut allowed = HashSet::new();
        if let Ok(raw) = fs::read_to_string(&persist_path)
            && let Ok(parsed) = serde_json::from_str::<PersistedPermissions>(&raw)
        {
            for t in parsed.allowed_tools {
                allowed.insert(t);
            }
        }
        Self {
            mode: Mutex::new(PermissionMode::Auto),
            session_allowed: Mutex::new(allowed),
            persist_path: Some(persist_path),
        }
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
            .lock()
            .map(|g| *g)
            .unwrap_or(PermissionMode::Auto)
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        if let Ok(mut g) = self.mode.lock() {
            *g = mode;
        }
    }

    pub fn evaluate(&self, tool_name: &str) -> PermissionDecision {
        let mode = self.mode();
        if mode == PermissionMode::Yolo {
            return PermissionDecision::Allow;
        }
        if let Ok(allowed) = self.session_allowed.lock()
            && allowed.contains(tool_name)
        {
            return PermissionDecision::Allow;
        }
        match mode {
            PermissionMode::Auto => match tool_name {
                "read_file" | "grep" | "glob" | "todo" | "edit_file" | "write_file" => {
                    PermissionDecision::Allow
                }
                _ => PermissionDecision::Ask,
            },
            PermissionMode::Ask => match tool_name {
                "read_file" | "grep" | "glob" | "todo" => PermissionDecision::Allow,
                _ => PermissionDecision::Ask,
            },
            PermissionMode::Yolo => PermissionDecision::Allow,
        }
    }

    /// 将指定工具加入自动放行白名单，并同步持久化到 `<workspace>/.gcode/permissions.json`。
    pub fn allow_for_session(&self, tool_name: &str) {
        let snapshot = if let Ok(mut allowed) = self.session_allowed.lock() {
            allowed.insert(tool_name.to_string());
            let mut list: Vec<String> = allowed.iter().cloned().collect();
            list.sort();
            Some(list)
        } else {
            None
        };

        if let (Some(path), Some(allowed_tools)) = (&self.persist_path, snapshot) {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(json) =
                serde_json::to_string_pretty(&PersistedPermissions { allowed_tools })
            {
                let _ = fs::write(path, json);
            }
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

/// 自动放行所有 `Ask` 级别工具（适用于测试或无头单次执行模式）。
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

    /// 由 UI 在用户按下确认/记住放行/拒绝后调用，唤醒等待中的工具执行协程。
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
    fn session_allowlist_promotes_ask_to_allow_and_persists() {
        let dir = std::env::temp_dir().join(format!("gcode_perm_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let policy = PermissionPolicy::load_for_workspace(&dir);
        // 默认 Auto 模式下文件编辑自动放行，shell 需询问
        assert_eq!(policy.evaluate("edit_file"), PermissionDecision::Allow);
        assert_eq!(policy.evaluate("shell"), PermissionDecision::Ask);

        policy.allow_for_session("shell");
        assert_eq!(policy.evaluate("shell"), PermissionDecision::Allow);

        // 重启后自动加载持久化白名单
        let reloaded = PermissionPolicy::load_for_workspace(&dir);
        assert_eq!(reloaded.evaluate("shell"), PermissionDecision::Allow);

        // 切换到 Yolo 模式放行一切；切换到 Ask 模式恢复严格询问
        reloaded.set_mode(PermissionMode::Yolo);
        assert_eq!(reloaded.evaluate("spawn_agent"), PermissionDecision::Allow);
        reloaded.set_mode(PermissionMode::Ask);
        assert_eq!(reloaded.evaluate("edit_file"), PermissionDecision::Ask);

        let _ = fs::remove_dir_all(&dir);
    }
}
