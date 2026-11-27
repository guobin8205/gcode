use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use gcode_models::{ModelProfile, ModelProvider};
use gcode_protocol::{EventKind, ToolDefinition};
use gcode_tools::fs::build_object_schema;
use gcode_tools::{Tool, ToolContext, ToolOutput, default_core_registry};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::event_store::EventStore;
use crate::governance::AutoApproveHandler;
use crate::loop_engine::{AgentRuntime, build_default_system_prompt};
use crate::snapshot::is_git_repo;

static SUBAGENT_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Deserialize)]
struct SpawnAgentArgs {
    /// 分配给子代理的独立任务描述（子代理看不到父对话历史，需提供完整背景）。
    task: String,
    /// 可选：是否在独立的 Git Worktree 隔离目录中执行（默认 false）。
    #[serde(default)]
    use_worktree: bool,
}

/// 隔离上下文子代理工具：启动独立的 `AgentRuntime` 执行子任务，支持可选 Git Worktree 隔离与级联取消（Abort Tree）。
pub struct SpawnAgentTool {
    provider: Arc<dyn ModelProvider>,
    profile: ModelProfile,
    /// 指向当前活跃父 Turn 的取消令牌，按下 Esc 时可级联取消运行中的子代理。
    active_cancel: Arc<RwLock<CancellationToken>>,
}

impl SpawnAgentTool {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        profile: ModelProfile,
        active_cancel: Arc<RwLock<CancellationToken>>,
    ) -> Self {
        Self {
            provider,
            profile,
            active_cancel,
        }
    }
}

struct WorktreeGuard {
    repo_root: PathBuf,
    worktree_dir: PathBuf,
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args([
                "worktree",
                "remove",
                "--force",
                &self.worktree_dir.to_string_lossy(),
            ])
            .output();
        let _ = fs::remove_dir_all(&self.worktree_dir);
    }
}

fn create_isolated_worktree(repo_root: &Path) -> Result<(PathBuf, WorktreeGuard), String> {
    if !is_git_repo(repo_root) {
        return Err("当前工作区不是 Git 仓库，无法创建 Worktree 隔离目录".to_string());
    }
    let seq = SUBAGENT_SEQ.fetch_add(1, Ordering::Relaxed);
    let wt_dir = std::env::temp_dir().join(format!("gcode_wt_{}_{seq}", std::process::id()));
    let _ = fs::remove_dir_all(&wt_dir);

    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "worktree",
            "add",
            "--detach",
            &wt_dir.to_string_lossy(),
            "HEAD",
        ])
        .output()
        .map_err(|e| format!("执行 git worktree add 失败: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "git worktree add 失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let guard = WorktreeGuard {
        repo_root: repo_root.to_path_buf(),
        worktree_dir: wt_dir.clone(),
    };
    Ok((wt_dir, guard))
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "spawn_agent".to_string(),
            description: "派生一个拥有独立上下文的子代理执行专项调研、测试或代码分析任务，避免污染主上下文。可选启用 Git Worktree 目录隔离。".to_string(),
            parameters: build_object_schema(
                vec![
                    (
                        "task",
                        json!({
                            "type": "string",
                            "description": "清晰、自包含的子任务说明（子代理无法看到父级对话历史）"
                        }),
                    ),
                    (
                        "use_worktree",
                        json!({
                            "type": "boolean",
                            "description": "是否在独立 Git Worktree 中运行（默认 false）"
                        }),
                    ),
                ],
                vec!["task"],
            ),
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> ToolOutput {
        let parsed: SpawnAgentArgs = match serde_json::from_value(args) {
            Ok(v) => v,
            Err(e) => {
                return ToolOutput {
                    content: format!("spawn_agent 参数解析失败: {e}"),
                    is_error: true,
                };
            }
        };

        let _wt_guard;
        let sub_workspace = if parsed.use_worktree {
            match create_isolated_worktree(&ctx.workspace_root) {
                Ok((dir, guard)) => {
                    _wt_guard = Some(guard);
                    dir
                }
                Err(e) => {
                    return ToolOutput {
                        content: e,
                        is_error: true,
                    };
                }
            }
        } else {
            _wt_guard = None;
            ctx.workspace_root.clone()
        };

        let seq = SUBAGENT_SEQ.fetch_add(1, Ordering::Relaxed);
        let log_path = std::env::temp_dir().join(format!(
            "gcode_subagent_{}_{seq}.jsonl",
            std::process::id()
        ));
        let store = match EventStore::open_or_create(&log_path) {
            Ok(s) => s,
            Err(e) => {
                return ToolOutput {
                    content: format!("初始化子代理事件存储失败: {e}"),
                    is_error: true,
                };
            }
        };

        // 子代理使用基础核心工具集（不递归注册 spawn_agent，防止无限嵌套）
        let sub_prompt = format!(
            "{}\n\n你正在作为子代理运行，请高效完成分配的专项任务，并直接给出简洁、结构化的最终结论。",
            build_default_system_prompt(&sub_workspace)
        );
        let mut sub_runtime = AgentRuntime::new(
            &sub_workspace,
            self.provider.clone(),
            self.profile.clone(),
            default_core_registry(),
            store,
            Arc::new(AutoApproveHandler),
            sub_prompt,
        );

        let child_cancel = {
            let guard = self.active_cancel.read().await;
            guard.child_token()
        };
        let (sub_tx, _sub_rx) = mpsc::unbounded_channel();

        let run_res = sub_runtime
            .run_turn(1, parsed.task, child_cancel, &sub_tx)
            .await;

        let final_reply = sub_runtime
            .store
            .active_events()
            .into_iter()
            .rev()
            .find_map(|ev| match ev.kind {
                EventKind::AssistantMessage { content, .. } if !content.trim().is_empty() => {
                    Some(content)
                }
                _ => None,
            })
            .unwrap_or_else(|| "（子代理未输出正文结论）".to_string());

        let worktree_note = if parsed.use_worktree {
            let status = Command::new("git")
                .arg("-C")
                .arg(&sub_workspace)
                .args(["status", "--short"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            if status.is_empty() {
                "\n\n[Worktree 隔离环境无文件变更，已自动清理]".to_string()
            } else {
                format!("\n\n[Worktree 隔离环境变更摘要（已清理临时目录）]\n{status}")
            }
        } else {
            String::new()
        };

        let _ = fs::remove_file(&log_path);

        match run_res {
            Ok(()) => ToolOutput {
                content: format!("[子代理汇报]\n{final_reply}{worktree_note}"),
                is_error: false,
            },
            Err(e) => ToolOutput {
                content: format!("子代理执行出错: {e}\n已收集部分结论: {final_reply}"),
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gcode_models::{ModelError, ModelFamily, ModelRequest, StreamChunk, provider::ModelStream};
    use gcode_protocol::TokenUsage;

    struct EchoSubProvider;

    #[async_trait]
    impl ModelProvider for EchoSubProvider {
        async fn stream_chat(&self, req: ModelRequest) -> Result<ModelStream, ModelError> {
            let task = req
                .messages
                .last()
                .map(|m| m.content.clone())
                .unwrap_or_default();
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::TextDelta(format!("已完成子任务: {task}"))),
                Ok(StreamChunk::Done(TokenUsage::default())),
            ])))
        }
    }

    #[tokio::test]
    async fn spawn_agent_executes_isolated_subtask_and_returns_report() {
        let dir = std::env::temp_dir().join(format!("gcode_sub_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let tool = SpawnAgentTool::new(
            Arc::new(EchoSubProvider),
            ModelProfile::for_model(ModelFamily::DeepSeek, "deepseek-chat"),
            Arc::new(RwLock::new(CancellationToken::new())),
        );
        let out = tool
            .execute(
                ToolContext {
                    workspace_root: dir.clone(),
                    call_id: "sub1".into(),
                },
                json!({ "task": "统计模块数量" }),
            )
            .await;

        assert!(!out.is_error);
        assert!(out.content.contains("已完成子任务: 统计模块数量"));

        let _ = fs::remove_dir_all(&dir);
    }
}
