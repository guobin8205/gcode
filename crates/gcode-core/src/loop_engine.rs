use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use gcode_models::{ModelError, ModelProfile, ModelProvider, ModelRequest, StreamChunk};
use gcode_protocol::{EventKind, Message, Role, ServerNotification, TokenUsage};
use gcode_tools::{ToolContext, ToolRegistry, discover_skills, format_skills_prompt};
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::compaction::{
    CompactionReport, DEFAULT_AUTO_COMPACT_THRESHOLD, DEFAULT_KEEP_RECENT_TOOLS,
    DEFAULT_MAX_OLD_TOOL_CHARS, DEFAULT_MICRO_COMPACT_THRESHOLD, append_memory_markdown,
    build_compaction_prompt, estimate_messages_tokens, find_compaction_split,
    micro_compact_messages, parse_compaction_output, render_events_for_compaction,
};
use crate::event_store::EventStore;
use crate::governance::{ApprovalHandler, ApprovalOutcome, PermissionDecision, PermissionPolicy};
use crate::hooks::{HookRunner, PreHookDecision};
use crate::snapshot::{capture_tree_snapshot, restore_tree_snapshot};
use crate::subagent::SpawnAgentTool;

/// 回滚/分叉操作的执行结果摘要。
#[derive(Debug, Clone)]
pub struct RollbackReport {
    pub target_turn_id: u64,
    pub restored_git_tree: Option<String>,
    pub message: String,
}

/// 默认最大自动重试次数（针对 429 限流、5xx 过载、网络抖动）。
const DEFAULT_MAX_RETRIES: usize = 4;
/// 默认指数退避基数（毫秒）：2s -> 4s -> 8s -> 16s。
const DEFAULT_RETRY_BASE_MS: u64 = 2_000;
/// 单次退避等待上限（秒），避免异常大的 `Retry-After` 无限阻塞。
const MAX_RETRY_DELAY_SECS: u64 = 60;

/// 无头单线程主循环，协调模型推理、工具调度与事件持久化。
pub struct AgentRuntime {
    pub workspace_root: PathBuf,
    pub provider: Arc<dyn ModelProvider>,
    pub profile: ModelProfile,
    pub tools: ToolRegistry,
    pub store: EventStore,
    pub policy: PermissionPolicy,
    pub hooks: HookRunner,
    pub approval: Arc<dyn ApprovalHandler>,
    pub system_prompt: String,
    pub max_retries: usize,
    pub retry_base_ms: u64,
    pub micro_compact_threshold: usize,
    pub auto_compact_threshold: usize,
    active_cancel: Arc<RwLock<CancellationToken>>,
}

impl AgentRuntime {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        provider: Arc<dyn ModelProvider>,
        profile: ModelProfile,
        tools: ToolRegistry,
        store: EventStore,
        approval: Arc<dyn ApprovalHandler>,
        system_prompt: impl Into<String>,
    ) -> Self {
        let workspace_root = workspace_root.into();
        let hooks = HookRunner::load_from_workspace(&workspace_root);
        Self {
            workspace_root,
            provider,
            profile,
            tools,
            store,
            policy: PermissionPolicy::default(),
            hooks,
            approval,
            system_prompt: system_prompt.into(),
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_ms: DEFAULT_RETRY_BASE_MS,
            micro_compact_threshold: DEFAULT_MICRO_COMPACT_THRESHOLD,
            auto_compact_threshold: DEFAULT_AUTO_COMPACT_THRESHOLD,
            active_cancel: Arc::new(RwLock::new(CancellationToken::new())),
        }
    }

    /// 启用 `spawn_agent` 隔离上下文子代理工具（主运行时按需调用，子代理不递归开启）。
    pub fn enable_subagent_tool(&mut self) {
        self.tools.register(Arc::new(SpawnAgentTool::new(
            self.provider.clone(),
            self.profile.clone(),
            self.active_cancel.clone(),
        )));
    }

    /// 执行单个用户 Turn，直到模型自然结束、出错或被 Abort Tree 令牌取消。
    pub async fn run_turn(
        &mut self,
        turn_id: u64,
        user_input: String,
        cancel: CancellationToken,
        tx: &mpsc::UnboundedSender<ServerNotification>,
    ) -> Result<()> {
        {
            let mut guard = self.active_cancel.write().await;
            *guard = cancel.clone();
        }
        let git_tree_hash = capture_tree_snapshot(&self.workspace_root).ok().flatten();
        let ev = self.store.append(
            turn_id,
            None,
            EventKind::TurnStarted {
                user_input,
                git_tree_hash,
            },
        )?;
        let _ = tx.send(ServerNotification::EventAppended { event: ev });

        let tool_defs = self.tools.definitions();
        let has_tools = !tool_defs.is_empty();
        let mut step_id = 1;

        loop {
            if cancel.is_cancelled() {
                let ev = self
                    .store
                    .append(turn_id, Some(step_id), EventKind::TurnFinished { aborted: true })?;
                let _ = tx.send(ServerNotification::EventAppended { event: ev });
                break;
            }

            // 自动检查是否触及上下文压缩阈值：优先执行两段式全量压缩（保留最近 2 个 Turn）
            let projected_before = self.store.project_messages();
            let est_tokens = estimate_messages_tokens(&projected_before);
            if est_tokens >= self.auto_compact_threshold {
                let _ = tx.send(ServerNotification::StatusMessage {
                    message: format!(
                        "上下文估算已达 ~{est_tokens} tokens，正在自动执行静默记忆落盘与上下文压缩…"
                    ),
                });
                let _ = self.compact(turn_id, 2, cancel.clone(), tx).await?;
                if cancel.is_cancelled() {
                    let ev = self.store.append(
                        turn_id,
                        Some(step_id),
                        EventKind::TurnFinished { aborted: true },
                    )?;
                    let _ = tx.send(ServerNotification::EventAppended { event: ev });
                    break;
                }
            }

            // 字节级稳定的前缀缓存拼装顺序：System -> Tools (BTreeMap) -> History
            let mut messages = vec![Message {
                role: Role::System,
                content: self.build_system_prompt(),
                reasoning_content: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
            }];
            let mut raw_history = self.store.project_messages();
            if estimate_messages_tokens(&raw_history) >= self.micro_compact_threshold {
                raw_history = micro_compact_messages(
                    raw_history,
                    DEFAULT_KEEP_RECENT_TOOLS,
                    DEFAULT_MAX_OLD_TOOL_CHARS,
                );
            }
            let history = self.profile.prepare_messages(&raw_history, has_tools);
            messages.extend(history);

            let req = ModelRequest {
                model_id: self.profile.model_id.clone(),
                messages,
                tools: tool_defs.clone(),
            };

            let Some(mut stream) = self
                .stream_chat_with_retry(req, &cancel, tx)
                .await?
            else {
                let ev = self.store.append(
                    turn_id,
                    Some(step_id),
                    EventKind::TurnFinished { aborted: true },
                )?;
                let _ = tx.send(ServerNotification::EventAppended { event: ev });
                break;
            };
            let mut content = String::new();
            let mut reasoning = String::new();
            let mut tool_calls = Vec::new();
            let mut usage = TokenUsage::default();

            while let Some(chunk) = tokio::select! {
                _ = cancel.cancelled() => None,
                next = stream.next() => next,
            } {
                match chunk? {
                    StreamChunk::TextDelta(delta) => {
                        content.push_str(&delta);
                        let _ = tx.send(ServerNotification::StreamDelta {
                            turn_id,
                            step_id,
                            text_delta: Some(delta),
                            reasoning_delta: None,
                        });
                    }
                    StreamChunk::ReasoningDelta(delta) => {
                        reasoning.push_str(&delta);
                        let _ = tx.send(ServerNotification::StreamDelta {
                            turn_id,
                            step_id,
                            text_delta: None,
                            reasoning_delta: Some(delta),
                        });
                    }
                    StreamChunk::ToolCallCompleted(call) => tool_calls.push(call),
                    StreamChunk::Done(u) => usage = u,
                }
            }

            let aborted_during_stream = cancel.is_cancelled();
            let persisted_tool_calls = if aborted_during_stream {
                // 流式阶段被打断时丢弃未执行的 tool_calls，避免历史中残留无对应 ToolResult 的孤立调用导致上游 400
                Vec::new()
            } else {
                tool_calls.clone()
            };
            let is_empty_response = content.trim().is_empty() && persisted_tool_calls.is_empty();
            if !aborted_during_stream || !content.is_empty() || !reasoning.is_empty() {
                let ev = self.store.append(
                    turn_id,
                    Some(step_id),
                    EventKind::AssistantMessage {
                        content,
                        reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
                        tool_calls: persisted_tool_calls.clone(),
                        usage,
                    },
                )?;
                let _ = tx.send(ServerNotification::EventAppended { event: ev });
            }

            if aborted_during_stream {
                let ev = self.store.append(
                    turn_id,
                    Some(step_id),
                    EventKind::TurnFinished { aborted: true },
                )?;
                let _ = tx.send(ServerNotification::EventAppended { event: ev });
                break;
            }

            if persisted_tool_calls.is_empty() {
                if is_empty_response {
                    let err_ev = self.store.append(
                        turn_id,
                        Some(step_id),
                        EventKind::Error {
                            message: "模型仅输出了思考过程而未生成正文或工具调用（可能因思考过长触及输出上限），建议重试或拆分任务。".to_string(),
                        },
                    )?;
                    let _ = tx.send(ServerNotification::EventAppended { event: err_ev });
                }
                let ev = self
                    .store
                    .append(turn_id, Some(step_id), EventKind::TurnFinished { aborted: false })?;
                let _ = tx.send(ServerNotification::EventAppended { event: ev });
                break;
            }

            for call in persisted_tool_calls {
                let (output, is_error) = if cancel.is_cancelled() {
                    ("工具执行已被用户打断".to_string(), true)
                } else {
                    self.dispatch_tool(&call, &cancel).await
                };
                let ev = self.store.append(
                    turn_id,
                    Some(step_id),
                    EventKind::ToolResult {
                        call_id: call.id,
                        tool_name: call.name,
                        output,
                        is_error,
                    },
                )?;
                let _ = tx.send(ServerNotification::EventAppended { event: ev });
            }

            if cancel.is_cancelled() {
                let ev = self.store.append(
                    turn_id,
                    Some(step_id),
                    EventKind::TurnFinished { aborted: true },
                )?;
                let _ = tx.send(ServerNotification::EventAppended { event: ev });
                break;
            }

            step_id += 1;
        }

        Ok(())
    }

    async fn dispatch_tool(
        &self,
        call: &gcode_protocol::ToolCall,
        cancel: &CancellationToken,
    ) -> (String, bool) {
        match self.policy.evaluate(&call.name) {
            PermissionDecision::Deny => {
                return (format!("权限策略拒绝执行工具 {}", call.name), true);
            }
            PermissionDecision::Ask => {
                let summary = format_approval_summary(&self.workspace_root, &call.name, &call.arguments);
                let outcome = tokio::select! {
                    _ = cancel.cancelled() => return ("工具审批已被用户打断".to_string(), true),
                    res = self.approval.request_approval(&call.id, &call.name, &summary) => res,
                };
                match outcome {
                    ApprovalOutcome::AllowOnce => {}
                    ApprovalOutcome::AllowSession => {
                        self.policy.allow_for_session(&call.name);
                    }
                    ApprovalOutcome::Deny => {
                        return (format!("用户拒绝执行工具 {}", call.name), true);
                    }
                }
            }
            PermissionDecision::Allow => {}
        }

        let Some(tool) = self.tools.get(&call.name) else {
            return (format!("未注册的工具: {}", call.name), true);
        };

        // 生命周期钩子：PreToolUse（支持外部脚本确定性拦截）
        if let PreHookDecision::Block(reason) = self
            .hooks
            .run_pre_tool_use(&self.workspace_root, &call.name, &call.arguments)
            .await
        {
            return (reason, true);
        }

        let ctx = ToolContext {
            workspace_root: self.workspace_root.clone(),
            call_id: call.id.clone(),
        };

        let (mut content, is_error) = tokio::select! {
            _ = cancel.cancelled() => ("工具执行已被用户取消".to_string(), true),
            out = tool.execute(ctx, call.arguments.clone()) => (out.content, out.is_error),
        };

        // 生命周期钩子：PostToolUse（可将格式化/检查输出附加回传给模型）
        if !cancel.is_cancelled()
            && let Some(hook_note) = self
                .hooks
                .run_post_tool_use(
                    &self.workspace_root,
                    &call.name,
                    &call.arguments,
                    &content,
                    is_error,
                )
                .await
        {
            content.push('\n');
            content.push_str(&hook_note);
        }

        (content, is_error)
    }

    /// 撤销最后一个活跃 Turn：同步回滚对话记忆与磁盘工作区快照。
    pub fn undo_last_turn(
        &mut self,
        tx: &mpsc::UnboundedSender<ServerNotification>,
    ) -> Result<RollbackReport> {
        let turns = self.store.active_turns();
        let Some(last_turn_id) = turns.last().map(|t| t.0) else {
            anyhow::bail!("当前没有可撤销的历史轮次");
        };
        self.rollback_to_turn(last_turn_id, tx)
    }

    /// 回滚/分叉至指定 `target_turn_id` 执行之前的状态（撤销 `target_turn_id` 及之后所有轮次），
    /// 若该轮次记录了 Git Tree 快照，同步还原工作区磁盘文件。
    pub fn rollback_to_turn(
        &mut self,
        target_turn_id: u64,
        tx: &mpsc::UnboundedSender<ServerNotification>,
    ) -> Result<RollbackReport> {
        let Some(snapshot_hash) = self.store.find_turn_snapshot(target_turn_id) else {
            anyhow::bail!("未找到活跃的 Turn #{target_turn_id}，可使用 `/turns` 查看可用轮次");
        };

        if let Some(hash) = &snapshot_hash {
            restore_tree_snapshot(&self.workspace_root, hash)?;
        }

        let ev = self.store.append(
            target_turn_id,
            None,
            EventKind::TurnForked {
                target_turn_id,
                restored_git_tree: snapshot_hash.clone(),
            },
        )?;
        let _ = tx.send(ServerNotification::EventAppended { event: ev });
        let _ = tx.send(ServerNotification::SessionRebuilt {
            events: self.store.active_events(),
        });

        let msg = match &snapshot_hash {
            Some(h) => format!(
                "已回滚至 Turn #{target_turn_id} 执行前，并还原磁盘快照 ({})。",
                &h[..h.len().min(8)]
            ),
            None => format!("已回滚对话记忆至 Turn #{target_turn_id} 执行前（非 Git 仓库，未改动磁盘文件）。"),
        };

        Ok(RollbackReport {
            target_turn_id,
            restored_git_tree: snapshot_hash,
            message: msg,
        })
    }

    /// 执行两段式全量上下文压缩：
    /// 1. 静默轮次调用模型提取长期记忆并生成精简摘要（Silent Memory Flush）；
    /// 2. 将提取出的长期记忆追加写入 `<workspace>/.gcode/MEMORY.md`；
    /// 3. 将 `EventKind::CompactionPerformed` 写入事件流，释放早期历史上下文。
    pub async fn compact(
        &mut self,
        turn_id: u64,
        keep_recent_turns: usize,
        cancel: CancellationToken,
        tx: &mpsc::UnboundedSender<ServerNotification>,
    ) -> Result<CompactionReport> {
        let before_tokens = estimate_messages_tokens(&self.store.project_messages());
        let active_events = self.store.active_events();
        let Some((first_kept_seq, to_compact)) =
            find_compaction_split(&active_events, keep_recent_turns)
        else {
            return Ok(CompactionReport {
                compacted: false,
                before_tokens,
                after_tokens: before_tokens,
                memory_saved: false,
                reason: "当前历史轮次较少，无需压缩。".to_string(),
            });
        };

        let previous_summary = active_events.iter().rev().find_map(|ev| match &ev.kind {
            EventKind::CompactionPerformed { summary, .. } => Some(summary.as_str()),
            _ => None,
        });
        let transcript = render_events_for_compaction(previous_summary, to_compact);
        let prompt = build_compaction_prompt(&transcript);

        let req = ModelRequest {
            model_id: self.profile.model_id.clone(),
            messages: vec![
                Message {
                    role: Role::System,
                    content: "你是严谨的上下文压缩与长期记忆整理助手，只输出事实摘要，不调用任何工具。"
                        .to_string(),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                },
                Message {
                    role: Role::User,
                    content: prompt,
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                },
            ],
            tools: Vec::new(),
        };

        let Some(mut stream) = self.stream_chat_with_retry(req, &cancel, tx).await? else {
            return Ok(CompactionReport {
                compacted: false,
                before_tokens,
                after_tokens: before_tokens,
                memory_saved: false,
                reason: "压缩已取消。".to_string(),
            });
        };

        let mut raw_output = String::new();
        while let Some(chunk) = tokio::select! {
            _ = cancel.cancelled() => None,
            next = stream.next() => next,
        } {
            if let StreamChunk::TextDelta(delta) = chunk? {
                raw_output.push_str(&delta);
            }
        }

        if cancel.is_cancelled() || raw_output.trim().is_empty() {
            return Ok(CompactionReport {
                compacted: false,
                before_tokens,
                after_tokens: before_tokens,
                memory_saved: false,
                reason: if cancel.is_cancelled() {
                    "压缩已取消。".to_string()
                } else {
                    "模型未返回有效摘要，保持原上下文不变。".to_string()
                },
            });
        }

        let (memory, summary) = parse_compaction_output(&raw_output);
        let mut memory_saved = false;
        if let Some(mem) = memory {
            append_memory_markdown(&self.workspace_root, &mem)?;
            memory_saved = true;
        }

        let ev = self.store.append(
            turn_id,
            None,
            EventKind::CompactionPerformed {
                summary,
                first_kept_seq,
            },
        )?;
        let _ = tx.send(ServerNotification::EventAppended { event: ev });

        let after_tokens = estimate_messages_tokens(&self.store.project_messages());
        Ok(CompactionReport {
            compacted: true,
            before_tokens,
            after_tokens,
            memory_saved,
            reason: format!(
                "上下文压缩完成：约 {before_tokens} → {after_tokens} tokens{}",
                if memory_saved {
                    "（已同步写入 .gcode/MEMORY.md）"
                } else {
                    ""
                }
            ),
        })
    }

    /// 带指数退避的模型请求重试；遇到 429 限流、5xx 过载或瞬时网络故障时自动等待并重试。
    /// 若等待期间用户取消（Esc / Ctrl+C），返回 `Ok(None)` 以便优雅结束当前轮次。
    async fn stream_chat_with_retry(
        &self,
        req: ModelRequest,
        cancel: &CancellationToken,
        tx: &mpsc::UnboundedSender<ServerNotification>,
    ) -> Result<Option<gcode_models::provider::ModelStream>> {
        let mut attempt = 0usize;
        loop {
            let call_res = tokio::select! {
                _ = cancel.cancelled() => return Ok(None),
                res = self.provider.stream_chat(req.clone()) => res,
            };

            match call_res {
                Ok(stream) => return Ok(Some(stream)),
                Err(err) if err.is_retryable() && attempt < self.max_retries => {
                    attempt += 1;
                    let delay = compute_retry_delay(&err, attempt, self.retry_base_ms);
                    let wait_secs = delay.as_secs_f64();
                    let label = match &err {
                        ModelError::RateLimited { .. } => "触发限流/配额限制",
                        ModelError::ServerOverloaded { status, .. } => {
                            if *status == 529 {
                                "上游服务过载 (529)"
                            } else {
                                "上游服务暂时不可用"
                            }
                        }
                        ModelError::Transport(_) => "网络连接抖动",
                        ModelError::Protocol(_) => "协议错误",
                    };
                    let detail_hint = match &err {
                        ModelError::RateLimited { detail, .. }
                        | ModelError::ServerOverloaded { detail, .. }
                            if !detail.is_empty() =>
                        {
                            let short: String = detail.chars().take(80).collect();
                            format!("（{short}）")
                        }
                        _ => String::new(),
                    };
                    let _ = tx.send(ServerNotification::StatusMessage {
                        message: format!(
                            "{label}{detail_hint}，{wait_secs:.0} 秒后自动第 {attempt}/{} 次重试（按 Esc 可取消）…",
                            self.max_retries
                        ),
                    });

                    tokio::select! {
                        _ = cancel.cancelled() => return Ok(None),
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn build_system_prompt(&self) -> String {
        if self.profile.system_nudge.is_empty() {
            self.system_prompt.clone()
        } else {
            format!("{}\n\n{}", self.system_prompt, self.profile.system_nudge)
        }
    }
}

/// 计算第 `attempt` 次（从 1 开始）重试的等待时长：优先采用服务端 `Retry-After`，否则指数退避。
fn compute_retry_delay(err: &ModelError, attempt: usize, base_ms: u64) -> Duration {
    if let Some(secs) = err.retry_after_secs() {
        return Duration::from_secs(secs.clamp(1, MAX_RETRY_DELAY_SECS));
    }
    let shift = attempt.saturating_sub(1).min(6) as u32;
    let ms = base_ms.saturating_mul(1u64 << shift);
    let max_ms = MAX_RETRY_DELAY_SECS * 1000;
    Duration::from_millis(ms.min(max_ms))
}

/// 将工具参数格式化为简洁可读的审批预览（含真实 Diff 行），避免在 UI 中展示原始转义 JSON。
fn format_approval_summary(workspace_root: &Path, tool_name: &str, args: &serde_json::Value) -> String {
    match tool_name {
        "shell" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("(空)");
            format!("执行命令: {cmd}")
        }
        "write_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let lines_cnt = content.lines().count();
            let exists = workspace_root.join(path).exists();
            let action = if exists { "覆盖文件" } else { "新建文件" };
            let mut out = vec![format!("{action}: {path}（共 {lines_cnt} 行）")];
            for l in content.lines().take(5) {
                out.push(format!("+ {l}"));
            }
            out.join("\n")
        }
        "edit_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            if let Some(start) = args.get("start_anchor").and_then(|v| v.as_str()) {
                let end = args.get("end_anchor").and_then(|v| v.as_str()).unwrap_or(start);
                let mut out = vec![format!("@@ {path}（锚点 {start}..{end}）@@")];
                // 尝试读取原文件目标行范围生成真实 '-' 对比行
                if let (Some(s_line), Some(e_line)) = (parse_anchor_line(start), parse_anchor_line(end))
                    && let Ok(disk) = std::fs::read_to_string(workspace_root.join(path))
                {
                    let disk_lines: Vec<&str> = disk.lines().collect();
                    let from = s_line.saturating_sub(1).min(disk_lines.len());
                    let to = e_line.min(disk_lines.len());
                    for l in disk_lines[from..to].iter().take(4) {
                        out.push(format!("- {l}"));
                    }
                }
                if let Some(new_c) = args.get("new_content").and_then(|v| v.as_str()) {
                    for l in new_c.lines().take(4) {
                        out.push(format!("+ {l}"));
                    }
                }
                out.join("\n")
            } else {
                let mut out = vec![format!("@@ {path}（文本替换）@@")];
                if let Some(old_s) = args.get("old_string").and_then(|v| v.as_str()) {
                    for l in old_s.lines().take(4) {
                        out.push(format!("- {l}"));
                    }
                }
                if let Some(new_s) = args.get("new_string").and_then(|v| v.as_str()) {
                    for l in new_s.lines().take(4) {
                        out.push(format!("+ {l}"));
                    }
                }
                out.join("\n")
            }
        }
        "spawn_agent" => {
            let task = args.get("task").and_then(|v| v.as_str()).unwrap_or("(空)");
            let wt = args
                .get("use_worktree")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mode = if wt { "Git Worktree 隔离" } else { "共享工作区" };
            format!("派生子代理（{mode}）\n任务: {task}")
        }
        _ => serde_json::to_string_pretty(args).unwrap_or_default(),
    }
}

fn parse_anchor_line(anchor: &str) -> Option<usize> {
    anchor.split(':').next()?.trim().parse::<usize>().ok()
}

/// 生成字节级稳定的基础系统提示词，并按需附加工作区 `GCODE.md` / `AGENTS.md` 规则。
pub fn build_default_system_prompt(workspace_root: &Path) -> String {
    let mut prompt = String::from(
        "你是 `gcode`，一个严谨、高效的终端编程 Agent。\n\
         核心工作纪律：\n\
         1. 修改任何代码前，先用 `grep`/`glob` 定位，再用 `read_file` 读取目标行范围。\n\
         2. `read_file` 会在每行前输出 `<行号>:<4位哈希>|` 锚点；调用 `edit_file` 时优先使用 `start_anchor`/`end_anchor` + `new_content` 进行精确修改，无需复述原代码。\n\
         3. 修改完成后，主动用 `shell` 运行相关构建或测试验证结果；只陈述你实际观察到的文件与命令输出。\n\
         4. 思考过程只做简短规划与决策，严禁在思考中提前写出大段完整代码；确定方案后直接调用 `write_file` 或 `edit_file` 落盘。\n\
         5. 保持回答简洁直接，默认使用中文与用户交流。",
    );

    for rule_file in ["GCODE.md", "AGENTS.md"] {
        let path = workspace_root.join(rule_file);
        if let Ok(content) = std::fs::read_to_string(&path)
            && !content.trim().is_empty()
        {
            prompt.push_str(&format!("\n\n## 项目规则 ({rule_file})\n{content}"));
        }
    }

    let memory_path = workspace_root.join(".gcode").join("MEMORY.md");
    if let Ok(content) = std::fs::read_to_string(&memory_path)
        && !content.trim().is_empty()
    {
        prompt.push_str(&format!("\n\n## 项目长期记忆 (.gcode/MEMORY.md)\n{content}"));
    }

    let skills = discover_skills(workspace_root);
    if let Some(skills_section) = format_skills_prompt(&skills) {
        prompt.push_str("\n\n");
        prompt.push_str(&skills_section);
    }

    prompt
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use gcode_models::{ModelError, ModelFamily, provider::ModelStream};
    use gcode_protocol::ToolCall;
    use gcode_tools::default_core_registry;
    use serde_json::json;

    use super::*;
    use crate::governance::AutoApproveHandler;

    struct MockProvider {
        step: AtomicUsize,
    }

    #[async_trait]
    impl ModelProvider for MockProvider {
        async fn stream_chat(&self, req: ModelRequest) -> Result<ModelStream, ModelError> {
            let current = self.step.fetch_add(1, Ordering::SeqCst);
            let items: Vec<Result<StreamChunk, ModelError>> = if current == 0 {
                vec![
                    Ok(StreamChunk::ReasoningDelta("先写入测试文件".to_string())),
                    Ok(StreamChunk::ToolCallCompleted(ToolCall {
                        id: "c1".to_string(),
                        name: "write_file".to_string(),
                        arguments: json!({"path": "demo.txt", "content": "ok\n"}),
                    })),
                    Ok(StreamChunk::Done(TokenUsage::default())),
                ]
            } else {
                assert!(req.messages.iter().any(|m| m.role == Role::Tool && m.content.contains("demo.txt")));
                vec![
                    Ok(StreamChunk::TextDelta("已完成写入".to_string())),
                    Ok(StreamChunk::Done(TokenUsage::default())),
                ]
            };
            Ok(Box::pin(futures::stream::iter(items)))
        }
    }

    #[tokio::test]
    async fn runtime_executes_tool_and_completes_turn() {
        let dir = std::env::temp_dir().join(format!("gcode_core_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let store = EventStore::open_or_create(dir.join("session.jsonl")).unwrap();
        let mut runtime = AgentRuntime::new(
            &dir,
            Arc::new(MockProvider { step: AtomicUsize::new(0) }),
            ModelProfile::for_model(ModelFamily::DeepSeek, "deepseek-reasoner"),
            default_core_registry(),
            store,
            Arc::new(AutoApproveHandler),
            "你是 gcode 编程助手。",
        );

        let (tx, _rx) = mpsc::unbounded_channel();
        runtime
            .run_turn(1, "创建 demo.txt".to_string(), CancellationToken::new(), &tx)
            .await
            .unwrap();

        let disk = std::fs::read_to_string(dir.join("demo.txt")).unwrap();
        assert_eq!(disk, "ok\n");
        assert!(runtime.store.events().iter().any(|e| matches!(e.kind, EventKind::ToolResult { is_error: false, .. })));

        let _ = std::fs::remove_dir_all(&dir);
    }

    struct FlakyRateLimitedProvider {
        attempts: AtomicUsize,
    }

    #[async_trait]
    impl ModelProvider for FlakyRateLimitedProvider {
        async fn stream_chat(&self, _req: ModelRequest) -> Result<ModelStream, ModelError> {
            let n = self.attempts.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                return Err(ModelError::RateLimited {
                    retry_after_secs: None,
                    detail: ": too many requests".to_string(),
                });
            }
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::TextDelta("重试后成功完成".to_string())),
                Ok(StreamChunk::Done(TokenUsage::default())),
            ])))
        }
    }

    #[tokio::test]
    async fn runtime_retries_on_rate_limit_and_succeeds() {
        let dir = std::env::temp_dir().join(format!("gcode_retry_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let store = EventStore::open_or_create(dir.join("session.jsonl")).unwrap();
        let mut runtime = AgentRuntime::new(
            &dir,
            Arc::new(FlakyRateLimitedProvider {
                attempts: AtomicUsize::new(0),
            }),
            ModelProfile::for_model(ModelFamily::Glm, "glm-5.2-local"),
            default_core_registry(),
            store,
            Arc::new(AutoApproveHandler),
            "你是 gcode 编程助手。",
        );
        // 单元测试中缩短退避基数以秒级完成
        runtime.retry_base_ms = 10;

        let (tx, mut rx) = mpsc::unbounded_channel();
        runtime
            .run_turn(1, "继续".to_string(), CancellationToken::new(), &tx)
            .await
            .unwrap();

        let mut status_count = 0usize;
        while let Ok(notif) = rx.try_recv() {
            if matches!(notif, ServerNotification::StatusMessage { .. }) {
                status_count += 1;
            }
        }
        assert_eq!(status_count, 2);
        assert!(runtime.store.events().iter().any(|e| matches!(
            &e.kind,
            EventKind::AssistantMessage { content, .. } if content == "重试后成功完成"
        )));

        let _ = std::fs::remove_dir_all(&dir);
    }

    struct CompactionMockProvider;

    #[async_trait]
    impl ModelProvider for CompactionMockProvider {
        async fn stream_chat(&self, _req: ModelRequest) -> Result<ModelStream, ModelError> {
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::TextDelta(
                    "## 长期记忆\n- 本项目使用 Rust 2024\n\n## 对话摘要\n已初始化工作区并完成基础测试。"
                        .to_string(),
                )),
                Ok(StreamChunk::Done(TokenUsage::default())),
            ])))
        }
    }

    #[tokio::test]
    async fn runtime_compact_flushes_memory_and_shrinks_projection() {
        let dir = std::env::temp_dir().join(format!("gcode_compact_rt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut store = EventStore::open_or_create(dir.join("session.jsonl")).unwrap();
        for t in 1..=3 {
            store
                .append(
                    t,
                    None,
                    EventKind::TurnStarted {
                        user_input: format!("第 {t} 轮任务：{}", "详细上下文".repeat(40)),
                        git_tree_hash: None,
                    },
                )
                .unwrap();
            store
                .append(
                    t,
                    Some(1),
                    EventKind::AssistantMessage {
                        content: format!("第 {t} 轮回复：{}", "执行详情".repeat(40)),
                        reasoning_content: None,
                        tool_calls: Vec::new(),
                        usage: TokenUsage::default(),
                    },
                )
                .unwrap();
        }

        let mut runtime = AgentRuntime::new(
            &dir,
            Arc::new(CompactionMockProvider),
            ModelProfile::for_model(ModelFamily::DeepSeek, "deepseek-chat"),
            default_core_registry(),
            store,
            Arc::new(AutoApproveHandler),
            "你是 gcode 编程助手。",
        );

        let (tx, _rx) = mpsc::unbounded_channel();
        let report = runtime
            .compact(3, 1, CancellationToken::new(), &tx)
            .await
            .unwrap();

        assert!(report.compacted);
        assert!(report.memory_saved);
        assert!(report.after_tokens < report.before_tokens);
        let mem = std::fs::read_to_string(dir.join(".gcode").join("MEMORY.md")).unwrap();
        assert!(mem.contains("本项目使用 Rust 2024"));

        let projected = runtime.store.project_messages();
        assert!(projected[0].content.contains("已初始化工作区并完成基础测试"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn runtime_undo_restores_disk_and_truncates_active_projection() {
        let dir = std::env::temp_dir().join(format!("gcode_undo_rt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init"])
            .output()
            .unwrap();
        assert!(init.status.success());
        std::fs::write(dir.join("base.txt"), "original\n").unwrap();

        let store = EventStore::open_or_create(dir.join(".gcode/sessions/s1.jsonl")).unwrap();
        let mut runtime = AgentRuntime::new(
            &dir,
            Arc::new(MockProvider {
                step: AtomicUsize::new(0),
            }),
            ModelProfile::for_model(ModelFamily::DeepSeek, "deepseek-chat"),
            default_core_registry(),
            store,
            Arc::new(AutoApproveHandler),
            "你是 gcode 编程助手。",
        );

        let (tx, _rx) = mpsc::unbounded_channel();
        runtime
            .run_turn(1, "创建 demo.txt".to_string(), CancellationToken::new(), &tx)
            .await
            .unwrap();
        assert!(dir.join("demo.txt").exists());
        assert!(!runtime.store.project_messages().is_empty());

        let report = runtime.undo_last_turn(&tx).unwrap();
        assert_eq!(report.target_turn_id, 1);
        assert!(report.restored_git_tree.is_some());
        assert!(!dir.join("demo.txt").exists());
        assert_eq!(std::fs::read_to_string(dir.join("base.txt")).unwrap(), "original\n");
        assert!(runtime.store.project_messages().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
