use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use gcode_core::{AgentRuntime, ApprovalOutcome, ChannelApprovalHandler, PermissionMode};
use gcode_protocol::{EventKind, ServerNotification, SessionEvent, TokenUsage, ToolCall};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::input::InputState;
use crate::markdown::{render_diff_lines, render_markdown_lines};

#[derive(Debug, Clone)]
enum TranscriptEntry {
    User(String),
    Thinking(String),
    Assistant(String),
    ToolResult {
        name: String,
        /// 从对应 `ToolCall` 参数中提取的可读动作描述（如 shell 命令、文件路径、搜索词）。
        action: Option<String>,
        output: String,
        is_error: bool,
    },
    SystemInfo(String),
}

#[derive(Debug, Clone)]
struct PendingApproval {
    call_id: String,
    tool_name: String,
    summary: String,
}

struct TuiState {
    model_id: String,
    workspace: String,
    entries: Vec<TranscriptEntry>,
    input: InputState,
    /// 当前排队或运行中的后台任务计数（`pending_tasks > 1` 表示有排队任务）。
    pending_tasks: usize,
    /// 等待依次执行的排队指令预览（独立渲染在底部状态区，不插入 `entries` 避免劈裂正在流式输出的回复）。
    queued_inputs: VecDeque<String>,
    /// 当前正在进行的实时活动描述与起始时间（用于渲染动态 Spinner 与耗时秒表）。
    activity: Option<(String, Instant)>,
    spinner_tick: usize,
    last_tick: Instant,
    show_thinking: bool,
    /// 是否展开全部历史工具调用明细（默认智能折叠连续大段工具轨迹，按 Ctrl+O 或 /tools 切换）。
    show_tools: bool,
    /// 缓存 `call_id -> 可读动作描述`，用于在 `ToolResult` 到来时展示真实命令/参数。
    call_actions: HashMap<String, String>,
    scroll_offset: u16,
    auto_scroll: bool,
    last_usage: TokenUsage,
    pending_approval: Option<PendingApproval>,
    permission_mode: PermissionMode,
    /// 是否捕获鼠标用于滚轮滚动与原生左键划选复制。
    mouse_capture: bool,
    /// 当前对话区内容矩形（不含四周边框），用于精准划选与自动剥离边框。
    transcript_inner: Rect,
    /// 鼠标左键拖拽选区 `(起点(x,y), 终点(x,y))`。
    selection: Option<((u16, u16), (u16, u16))>,
    turn_counter: u64,
}

impl TuiState {
    fn is_running(&self) -> bool {
        self.pending_tasks > 0
    }

    fn from_store(
        store: &gcode_core::EventStore,
        model_id: String,
        workspace: String,
        permission_mode: PermissionMode,
    ) -> Self {
        let turn_counter = store.last_turn_id();
        let active = store.active_events();
        let (entries, last_usage) = build_entries_from_events(&active, turn_counter > 0);

        Self {
            model_id,
            workspace,
            entries,
            input: InputState::default(),
            pending_tasks: 0,
            queued_inputs: VecDeque::new(),
            activity: None,
            spinner_tick: 0,
            last_tick: Instant::now(),
            show_thinking: false,
            show_tools: false,
            call_actions: HashMap::new(),
            scroll_offset: 0,
            auto_scroll: true,
            last_usage,
            pending_approval: None,
            permission_mode,
            mouse_capture: true,
            transcript_inner: Rect::default(),
            selection: None,
            turn_counter,
        }
    }

    fn rebuild_from_events(&mut self, events: &[SessionEvent]) {
        let (entries, last_usage) = build_entries_from_events(events, false);
        self.entries = entries;
        self.last_usage = last_usage;
        self.scroll_offset = 0;
        self.auto_scroll = true;
    }

    fn set_activity(&mut self, label: impl Into<String>) {
        let label = label.into();
        if self.activity.as_ref().map(|(s, _)| s.as_str()) != Some(label.as_str()) {
            self.activity = Some((label, Instant::now()));
        }
    }

    fn append_thinking_delta(&mut self, delta: &str) {
        if let Some(TranscriptEntry::Thinking(buf)) = self.entries.last_mut() {
            buf.push_str(delta);
        } else {
            self.entries.push(TranscriptEntry::Thinking(delta.to_string()));
        }
    }

    fn append_text_delta(&mut self, delta: &str) {
        if let Some(TranscriptEntry::Assistant(buf)) = self.entries.last_mut() {
            buf.push_str(delta);
        } else {
            self.entries.push(TranscriptEntry::Assistant(delta.to_string()));
        }
    }
}

fn build_entries_from_events(
    events: &[SessionEvent],
    show_resume_banner: bool,
) -> (Vec<TranscriptEntry>, TokenUsage) {
    let mut entries = vec![TranscriptEntry::SystemInfo(
        "欢迎使用 gcode！回车发送；Tab 折叠思考；Esc 中断；输入 /help 查看全部命令与快捷键。".to_string(),
    )];
    let mut last_usage = TokenUsage::default();
    let active_turns = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::TurnStarted { .. }))
        .count();

    if show_resume_banner && active_turns > 0 {
        entries.push(TranscriptEntry::SystemInfo(format!(
            "已恢复历史会话（当前活跃 {active_turns} 个轮次）："
        )));
    }

    let mut call_actions: HashMap<String, String> = HashMap::new();
    for ev in events {
        match &ev.kind {
            EventKind::TurnStarted { user_input, .. } => {
                entries.push(TranscriptEntry::User(user_input.clone()));
            }
            EventKind::AssistantMessage {
                content,
                reasoning_content,
                tool_calls,
                usage,
                ..
            } => {
                for tc in tool_calls {
                    call_actions.insert(tc.id.clone(), summarize_tool_call(tc));
                }
                if let Some(r) = reasoning_content
                    && !r.is_empty()
                {
                    entries.push(TranscriptEntry::Thinking(r.clone()));
                }
                if !content.is_empty() {
                    entries.push(TranscriptEntry::Assistant(content.clone()));
                }
                last_usage = usage.clone();
            }
            EventKind::ToolResult {
                call_id,
                tool_name,
                output,
                is_error,
                ..
            } => {
                entries.push(TranscriptEntry::ToolResult {
                    name: tool_name.clone(),
                    action: call_actions.remove(call_id),
                    output: output.clone(),
                    is_error: *is_error,
                });
            }
            EventKind::TurnFinished { aborted: true } => {
                entries.push(TranscriptEntry::SystemInfo("（该轮次曾被中断）".to_string()));
            }
            EventKind::CompactionPerformed { .. } => {
                entries.push(TranscriptEntry::SystemInfo(
                    "── [已执行上下文压缩与长期记忆落盘] ──".to_string(),
                ));
            }
            _ => {}
        }
    }

    (entries, last_usage)
}

enum WorkerTask {
    Prompt {
        turn_id: u64,
        prompt: String,
        cancel: CancellationToken,
    },
    Compact {
        turn_id: u64,
        cancel: CancellationToken,
    },
    Undo,
    Fork {
        target_turn_id: u64,
    },
    ListTurns,
}

/// 启动完整的 Ratatui 交互式终端界面，并与 `AgentRuntime` 异步联动。
pub async fn run_tui_with_runtime(
    mut runtime: AgentRuntime,
    approval_broker: Option<Arc<ChannelApprovalHandler>>,
    notif_tx: mpsc::UnboundedSender<ServerNotification>,
    mut notif_rx: mpsc::UnboundedReceiver<ServerNotification>,
) -> Result<()> {
    let model_id = runtime.profile.model_id.clone();
    let workspace = runtime.workspace_root.display().to_string();
    let policy = runtime.policy.clone();
    let initial_mode = policy.mode();
    let mut state = TuiState::from_store(&runtime.store, model_id, workspace, initial_mode);

    let (task_tx, mut task_rx) = mpsc::unbounded_channel::<WorkerTask>();
    let runtime_notif_tx = notif_tx.clone();
    tokio::spawn(async move {
        while let Some(task) = task_rx.recv().await {
            match task {
                WorkerTask::Prompt {
                    turn_id,
                    prompt,
                    cancel,
                } => {
                    if let Err(e) = runtime.run_turn(turn_id, prompt, cancel, &runtime_notif_tx).await {
                        let _ = runtime_notif_tx.send(ServerNotification::EventAppended {
                            event: gcode_protocol::SessionEvent {
                                seq: 0,
                                timestamp: chrono::Utc::now(),
                                turn_id,
                                step_id: None,
                                ignorable: true,
                                kind: EventKind::Error {
                                    message: e.to_string(),
                                },
                            },
                        });
                        let _ = runtime_notif_tx.send(ServerNotification::EventAppended {
                            event: gcode_protocol::SessionEvent {
                                seq: 0,
                                timestamp: chrono::Utc::now(),
                                turn_id,
                                step_id: None,
                                ignorable: true,
                                kind: EventKind::TurnFinished { aborted: true },
                            },
                        });
                    }
                }
                WorkerTask::Compact { turn_id, cancel } => {
                    let _ = runtime_notif_tx.send(ServerNotification::StatusMessage {
                        message: "正在执行静默记忆落盘与上下文压缩…".to_string(),
                    });
                    match runtime.compact(turn_id, 1, cancel, &runtime_notif_tx).await {
                        Ok(report) => {
                            let _ = runtime_notif_tx.send(ServerNotification::StatusMessage {
                                message: report.reason,
                            });
                        }
                        Err(e) => {
                            let _ = runtime_notif_tx.send(ServerNotification::EventAppended {
                                event: gcode_protocol::SessionEvent {
                                    seq: 0,
                                    timestamp: chrono::Utc::now(),
                                    turn_id,
                                    step_id: None,
                                    ignorable: true,
                                    kind: EventKind::Error {
                                        message: format!("压缩失败: {e}"),
                                    },
                                },
                            });
                        }
                    }
                    let _ = runtime_notif_tx.send(ServerNotification::EventAppended {
                        event: gcode_protocol::SessionEvent {
                            seq: 0,
                            timestamp: chrono::Utc::now(),
                            turn_id,
                            step_id: None,
                            ignorable: true,
                            kind: EventKind::TurnFinished { aborted: false },
                        },
                    });
                }
                WorkerTask::Undo => {
                    match runtime.undo_last_turn(&runtime_notif_tx) {
                        Ok(report) => {
                            let _ = runtime_notif_tx.send(ServerNotification::StatusMessage {
                                message: report.message,
                            });
                        }
                        Err(e) => {
                            let _ = runtime_notif_tx.send(ServerNotification::StatusMessage {
                                message: format!("撤销失败: {e}"),
                            });
                        }
                    }
                    let _ = runtime_notif_tx.send(ServerNotification::EventAppended {
                        event: gcode_protocol::SessionEvent {
                            seq: 0,
                            timestamp: chrono::Utc::now(),
                            turn_id: 0,
                            step_id: None,
                            ignorable: true,
                            kind: EventKind::TurnFinished { aborted: false },
                        },
                    });
                }
                WorkerTask::Fork { target_turn_id } => {
                    match runtime.rollback_to_turn(target_turn_id, &runtime_notif_tx) {
                        Ok(report) => {
                            let _ = runtime_notif_tx.send(ServerNotification::StatusMessage {
                                message: report.message,
                            });
                        }
                        Err(e) => {
                            let _ = runtime_notif_tx.send(ServerNotification::StatusMessage {
                                message: format!("回滚失败: {e}"),
                            });
                        }
                    }
                    let _ = runtime_notif_tx.send(ServerNotification::EventAppended {
                        event: gcode_protocol::SessionEvent {
                            seq: 0,
                            timestamp: chrono::Utc::now(),
                            turn_id: 0,
                            step_id: None,
                            ignorable: true,
                            kind: EventKind::TurnFinished { aborted: false },
                        },
                    });
                }
                WorkerTask::ListTurns => {
                    let turns = runtime.store.active_turns();
                    let msg = if turns.is_empty() {
                        "当前没有活跃的历史轮次。".to_string()
                    } else {
                        let mut lines = vec!["当前活跃轮次（可用 `/fork <轮次号>` 回滚至该轮之前）：".to_string()];
                        for (tid, input, has_snap) in turns {
                            let snap_tag = if has_snap { "[含快照]" } else { "[无快照]" };
                            let preview: String = input.chars().take(48).collect();
                            lines.push(format!("  #{tid} {snap_tag} {preview}"));
                        }
                        lines.join("\n")
                    };
                    let _ = runtime_notif_tx.send(ServerNotification::StatusMessage { message: msg });
                    let _ = runtime_notif_tx.send(ServerNotification::EventAppended {
                        event: gcode_protocol::SessionEvent {
                            seq: 0,
                            timestamp: chrono::Utc::now(),
                            turn_id: 0,
                            step_id: None,
                            ignorable: true,
                            kind: EventKind::TurnFinished { aborted: false },
                        },
                    });
                }
            }
        }
    });

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // 默认开启 Bracketed Paste（粘贴多行文本不会逐行自动提交）和鼠标滚轮捕获
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    set_mouse_capture(&mut stdout, true)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut cancel_queue: VecDeque<CancellationToken> = VecDeque::new();
    let mut last_char_at: Option<Instant> = None;
    let mut in_paste_burst = false;

    let res = loop {
        if in_paste_burst && last_char_at.is_some_and(|t| t.elapsed() >= Duration::from_millis(40)) {
            in_paste_burst = false;
        }
        if state.is_running() && state.last_tick.elapsed() >= Duration::from_millis(90) {
            state.spinner_tick = state.spinner_tick.wrapping_add(1);
            state.last_tick = Instant::now();
        }

        while let Ok(notif) = notif_rx.try_recv() {
            match notif {
                ServerNotification::StreamDelta {
                    text_delta,
                    reasoning_delta,
                    ..
                } => {
                    if let Some(r) = reasoning_delta {
                        state.set_activity("模型思考中…");
                        state.append_thinking_delta(&r);
                    }
                    if let Some(t) = text_delta {
                        state.set_activity("正在生成回复…");
                        state.append_text_delta(&t);
                    }
                    state.auto_scroll = true;
                }
                ServerNotification::ToolStarted {
                    tool_name, action, ..
                } => {
                    let label = if action.is_empty() {
                        format!("正在执行 {tool_name}…")
                    } else {
                        format!("正在执行 {tool_name}: {action}")
                    };
                    state.activity = Some((label, Instant::now()));
                    state.auto_scroll = true;
                }
                ServerNotification::ApprovalRequired {
                    call_id,
                    tool_name,
                    summary,
                } => {
                    state.set_activity(format!("等待权限确认: {tool_name}"));
                    state.pending_approval = Some(PendingApproval {
                        call_id,
                        tool_name,
                        summary,
                    });
                }
                ServerNotification::StatusMessage { message } => {
                    for line in message.lines() {
                        state.entries.push(TranscriptEntry::SystemInfo(line.to_string()));
                    }
                    state.auto_scroll = true;
                }
                ServerNotification::SessionRebuilt { events } => {
                    state.rebuild_from_events(&events);
                }
                ServerNotification::EventAppended { event } => match event.kind {
                    EventKind::TurnStarted { user_input, .. } => {
                        if state.queued_inputs.front().map(String::as_str) == Some(user_input.as_str()) {
                            state.queued_inputs.pop_front();
                        }
                        let already_shown = matches!(
                            state.entries.last(),
                            Some(TranscriptEntry::User(s)) if s == &user_input
                        );
                        if !already_shown {
                            state.entries.push(TranscriptEntry::User(user_input));
                        }
                        state.activity = Some(("等待模型响应…".to_string(), Instant::now()));
                        state.auto_scroll = true;
                    }
                    EventKind::AssistantMessage {
                        usage, tool_calls, ..
                    } => {
                        for tc in tool_calls {
                            state.call_actions.insert(tc.id.clone(), summarize_tool_call(&tc));
                        }
                        state.last_usage = usage;
                    }
                    EventKind::ToolResult {
                        call_id,
                        tool_name,
                        output,
                        is_error,
                        ..
                    } => {
                        let action = state.call_actions.remove(&call_id);
                        state.entries.push(TranscriptEntry::ToolResult {
                            name: tool_name,
                            action,
                            output,
                            is_error,
                        });
                        state.activity = Some(("等待模型继续推理…".to_string(), Instant::now()));
                        state.auto_scroll = true;
                    }
                    EventKind::TurnFinished { aborted } => {
                        state.pending_tasks = state.pending_tasks.saturating_sub(1);
                        let _ = cancel_queue.pop_front();
                        if state.pending_tasks == 0 {
                            state.activity = None;
                            state.queued_inputs.clear();
                        } else {
                            state.activity = Some(("开始执行下一条排队指令…".to_string(), Instant::now()));
                        }
                        if aborted {
                            state.entries.push(TranscriptEntry::SystemInfo("（上一轮执行已中断）".to_string()));
                        }
                        state.auto_scroll = true;
                    }
                    EventKind::Error { message } => {
                        state.entries.push(TranscriptEntry::SystemInfo(format!("错误: {message}")));
                        state.auto_scroll = true;
                    }
                    _ => {}
                },
            }
        }

        if let Err(e) = terminal.draw(|frame| draw_ui(frame, &mut state)) {
            break Err(e.into());
        }

        if event::poll(Duration::from_millis(20))? {
            let ev = event::read()?;
            if let Event::Mouse(mouse) = ev {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        state.selection = None;
                        state.auto_scroll = false;
                        state.scroll_offset = state.scroll_offset.saturating_sub(3);
                    }
                    MouseEventKind::ScrollDown => {
                        state.selection = None;
                        state.scroll_offset = state.scroll_offset.saturating_add(3);
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if point_in_rect(mouse.column, mouse.row, state.transcript_inner) {
                            state.selection = Some(((mouse.column, mouse.row), (mouse.column, mouse.row)));
                        } else {
                            state.selection = None;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some((start, _)) = state.selection {
                            let cx = clamp_x(mouse.column, state.transcript_inner);
                            let cy = clamp_y(mouse.row, state.transcript_inner);
                            state.selection = Some((start, (cx, cy)));
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some((start, end)) = state.selection.take()
                            && start != end
                        {
                            let selected = extract_selected_text(
                                terminal.current_buffer_mut(),
                                state.transcript_inner,
                                start,
                                end,
                            );
                            if !selected.trim().is_empty() {
                                match copy_to_clipboard(&selected) {
                                    Ok(()) => state.entries.push(TranscriptEntry::SystemInfo(
                                        format!("已复制选中内容到剪贴板（{} 字）。", selected.chars().count()),
                                    )),
                                    Err(e) => state.entries.push(TranscriptEntry::SystemInfo(
                                        format!("复制选中内容失败: {e}"),
                                    )),
                                }
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if let Event::Paste(pasted) = ev {
                if state.pending_approval.is_none() {
                    state.input.insert_paste(&pasted);
                }
                continue;
            }
            if let Event::Key(key) = ev
                && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                for c in cancel_queue.drain(..) {
                    c.cancel();
                }
                break Ok(());
            }

            if let Some(approval) = state.pending_approval.clone() {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        if let Some(broker) = &approval_broker {
                            broker.resolve(&approval.call_id, ApprovalOutcome::AllowOnce);
                        }
                        state.pending_approval = None;
                    }
                    KeyCode::Char('a') | KeyCode::Char('A') => {
                        if let Some(broker) = &approval_broker {
                            broker.resolve(&approval.call_id, ApprovalOutcome::AllowSession);
                        }
                        state.pending_approval = None;
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        if let Some(broker) = &approval_broker {
                            broker.resolve(&approval.call_id, ApprovalOutcome::Deny);
                        }
                        state.pending_approval = None;
                    }
                    KeyCode::BackTab => {
                        let next = state.permission_mode.cycle();
                        state.permission_mode = next;
                        policy.set_mode(next);
                        state.entries.push(TranscriptEntry::SystemInfo(format!(
                            "已实时切换权限模式 → {}",
                            next.label()
                        )));
                        if next == PermissionMode::Yolo {
                            if let Some(broker) = &approval_broker {
                                broker.resolve(&approval.call_id, ApprovalOutcome::AllowOnce);
                            }
                            state.pending_approval = None;
                        }
                    }
                    KeyCode::Up => {
                        state.auto_scroll = false;
                        state.scroll_offset = state.scroll_offset.saturating_sub(2);
                    }
                    KeyCode::Down => {
                        state.scroll_offset = state.scroll_offset.saturating_add(2);
                    }
                    KeyCode::PageUp => {
                        state.auto_scroll = false;
                        state.scroll_offset = state.scroll_offset.saturating_sub(10);
                    }
                    KeyCode::PageDown => {
                        state.scroll_offset = state.scroll_offset.saturating_add(10);
                    }
                    _ => {}
                }
                continue;
            }

            match key.code {
                KeyCode::Esc => {
                    if let Some(c) = cancel_queue.front() {
                        c.cancel();
                        state.entries.push(TranscriptEntry::SystemInfo(
                            "正在中断当前执行…".to_string(),
                        ));
                        state.auto_scroll = true;
                    } else if !state.input.is_empty() {
                        // 空闲状态下按 Esc 一键清空当前输入框（方便误粘贴大段文本时快速重来）
                        state.input.clear_all();
                    }
                }
                KeyCode::Tab => {
                    state.show_thinking = !state.show_thinking;
                }
                KeyCode::F(2) => {
                    state.mouse_capture = !state.mouse_capture;
                    let _ = set_mouse_capture(terminal.backend_mut(), state.mouse_capture);
                    if state.mouse_capture {
                        state.entries.push(TranscriptEntry::SystemInfo(
                            "鼠标模式：滚轮滚动对话（按住 Shift+左键可划选复制，F2 切换）。".to_string(),
                        ));
                    } else {
                        state.entries.push(TranscriptEntry::SystemInfo(
                            "鼠标模式：自由左键划选复制（按 F2 恢复滚轮滚动）。".to_string(),
                        ));
                    }
                    state.auto_scroll = true;
                }
                KeyCode::BackTab => {
                    let next = state.permission_mode.cycle();
                    state.permission_mode = next;
                    policy.set_mode(next);
                    state.entries.push(TranscriptEntry::SystemInfo(format!(
                        "已实时切换权限模式 → {}（auto=编辑免问 / yolo=全自动 / ask=严格询问）",
                        next.label()
                    )));
                    state.auto_scroll = true;
                }
                KeyCode::Up
                    if key.modifiers.contains(KeyModifiers::SHIFT)
                        || key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    state.auto_scroll = false;
                    state.scroll_offset = state.scroll_offset.saturating_sub(2);
                }
                KeyCode::Down
                    if key.modifiers.contains(KeyModifiers::SHIFT)
                        || key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    state.scroll_offset = state.scroll_offset.saturating_add(2);
                }
                KeyCode::Up => {
                    state.input.history_prev();
                }
                KeyCode::Down => {
                    state.input.history_next();
                }
                KeyCode::PageUp => {
                    state.auto_scroll = false;
                    state.scroll_offset = state.scroll_offset.saturating_sub(10);
                }
                KeyCode::PageDown => {
                    state.scroll_offset = state.scroll_offset.saturating_add(10);
                }
                KeyCode::Left => {
                    state.input.move_left();
                }
                KeyCode::Right => {
                    state.input.move_right();
                }
                KeyCode::Home => {
                    state.input.move_home();
                }
                KeyCode::End => {
                    state.input.move_end();
                }
                KeyCode::Delete => {
                    state.input.delete();
                }
                KeyCode::Backspace => {
                    state.input.backspace();
                }
                KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.input.move_home();
                }
                KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.input.move_end();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.input.clear_to_start();
                }
                KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.input.clear_to_end();
                }
                KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.show_tools = !state.show_tools;
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.input.history_prev();
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.input.history_next();
                }
                KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.input.insert_char('\n');
                }
                KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Ok(text) = paste_from_clipboard() {
                        state.input.insert_paste(&text);
                    }
                }
                KeyCode::Char(c) => {
                    let now = Instant::now();
                    // Windows 右键/Ctrl+Shift+V 粘贴会产生高速连续按键队列：
                    // 若后续队列立刻有更多按键，直接一次性收拢为整段文本并折叠成 `[Pasted text #N +M lines]` 块！
                    if event::poll(Duration::ZERO).unwrap_or(false) {
                        let mut burst = String::new();
                        burst.push(c);
                        while event::poll(Duration::from_millis(8)).unwrap_or(false) {
                            match event::read()? {
                                Event::Key(k)
                                    if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                                {
                                    match k.code {
                                        KeyCode::Char(ch) => burst.push(ch),
                                        KeyCode::Enter => burst.push('\n'),
                                        KeyCode::Tab => burst.push('\t'),
                                        _ => break,
                                    }
                                }
                                Event::Paste(p) => burst.push_str(&p),
                                _ => {}
                            }
                        }
                        in_paste_burst = true;
                        last_char_at = Some(Instant::now());
                        state.input.insert_paste(&burst);
                    } else {
                        if last_char_at
                            .is_some_and(|t| now.duration_since(t) < Duration::from_millis(12))
                        {
                            in_paste_burst = true;
                        }
                        last_char_at = Some(now);
                        state.input.insert_char(c);
                    }
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    state.input.insert_char('\n');
                }
                KeyCode::Enter => {
                    // Windows 下终端右键/Ctrl+Shift+V 粘贴会以爆发式 Key 事件涌入队列：
                    // 若 8ms 内紧跟后续按键，或刚处于高速粘贴流中，则把 Enter 当作换行而非提交。
                    let is_paste_newline = !key.modifiers.contains(KeyModifiers::CONTROL)
                        && (event::poll(Duration::from_millis(8)).unwrap_or(false)
                            || (in_paste_burst
                                && last_char_at
                                    .is_some_and(|t| t.elapsed() < Duration::from_millis(30))));
                    if is_paste_newline {
                        in_paste_burst = true;
                        last_char_at = Some(Instant::now());
                        state.input.insert_char('\n');
                        continue;
                    }
                    in_paste_burst = false;
                    if state.input.text().trim().is_empty() {
                        continue;
                    }
                    let raw_input = state.input.take_and_record();
                    if matches!(raw_input.as_str(), "/quit" | "/exit") {
                        for c in cancel_queue.drain(..) {
                            c.cancel();
                        }
                        break Ok(());
                    }

                    // 纯本地界面命令无需打断或排队
                    match raw_input.as_str() {
                        "/help" => {
                            for l in [
                                "可用命令：",
                                "  /compact         执行静默记忆落盘与上下文压缩",
                                "  /undo            撤销最近一轮对话并还原 Git 磁盘快照",
                                "  /turns           查看所有活跃历史轮次与快照状态",
                                "  /fork <轮次号>   回滚至指定轮次执行前（别名 /rollback）",
                                "  /yolo            切换为全自动免确认模式（等同 /mode yolo）",
                                "  /mode <模式>     切换权限模式：auto（默认编辑免问）| yolo | ask",
                                "  /copy [all]      复制最后一条助手回复（加 all 复制全部对话记录）到剪贴板",
                                "  /mouse           开关鼠标滚轮捕获（等同 F2；开启时按住 Shift+左键可复制）",
                                "  /tools           展开/折叠工具调用执行明细（等同 Ctrl+O）",
                                "  /thinking        切换思考过程展开/折叠（等同 Tab）",
                                "  /clear           清空当前屏幕显示（不影响会话历史）",
                                "  /quit            退出 gcode",
                                "运行中发指令：Enter=加入队列等待完成 | Ctrl+Enter 或 `!指令`=立即打断切入 | Esc=中断",
                                "快捷键：滚轮/PgUp/PgDn 滚动 | ↑/↓ 历史 | Shift+左键复制(或 F2) | Shift+Tab 权限模式",
                            ] {
                                state.entries.push(TranscriptEntry::SystemInfo(l.to_string()));
                            }
                            state.auto_scroll = true;
                            continue;
                        }
                        "/clear" => {
                            state.entries.clear();
                            state.scroll_offset = 0;
                            continue;
                        }
                        "/thinking" => {
                            state.show_thinking = !state.show_thinking;
                            continue;
                        }
                        "/tools" => {
                            state.show_tools = !state.show_tools;
                            continue;
                        }
                        "/copy" | "/copy all" => {
                            let is_all = raw_input == "/copy all";
                            let text_opt = if is_all {
                                let dump = render_plain_transcript(&state.entries);
                                (!dump.trim().is_empty()).then_some(dump)
                            } else {
                                state.entries.iter().rev().find_map(|e| match e {
                                    TranscriptEntry::Assistant(s) if !s.trim().is_empty() => {
                                        Some(s.clone())
                                    }
                                    _ => None,
                                })
                            };
                            match text_opt {
                                Some(text) => match copy_to_clipboard(&text) {
                                    Ok(()) => state.entries.push(TranscriptEntry::SystemInfo(
                                        format!(
                                            "已复制{}到剪贴板（{} 字）。",
                                            if is_all { "全部对话记录" } else { "最后一条助手回复" },
                                            text.chars().count()
                                        ),
                                    )),
                                    Err(e) => state.entries.push(TranscriptEntry::SystemInfo(
                                        format!("复制失败: {e}"),
                                    )),
                                },
                                None => state.entries.push(TranscriptEntry::SystemInfo(
                                    "当前没有可复制的内容。".to_string(),
                                )),
                            }
                            state.auto_scroll = true;
                            continue;
                        }
                        "/mouse" => {
                            state.mouse_capture = !state.mouse_capture;
                            let _ = set_mouse_capture(terminal.backend_mut(), state.mouse_capture);
                            if state.mouse_capture {
                                state.entries.push(TranscriptEntry::SystemInfo(
                                    "已开启鼠标滚轮滚动（划选复制需按住 Shift+左键，或再次按 F2 / 输入 /mouse 关闭）。".to_string(),
                                ));
                            } else {
                                state.entries.push(TranscriptEntry::SystemInfo(
                                    "已关闭鼠标捕获，现在可直接用鼠标左键划选复制文本（按 F2 恢复滚轮滚动）。".to_string(),
                                ));
                            }
                            state.auto_scroll = true;
                            continue;
                        }
                        "/yolo" => {
                            state.permission_mode = PermissionMode::Yolo;
                            policy.set_mode(PermissionMode::Yolo);
                            state.entries.push(TranscriptEntry::SystemInfo(
                                "已实时切换权限模式 → yolo（所有工具自动放行，按 Shift+Tab 可切回）".to_string(),
                            ));
                            state.auto_scroll = true;
                            continue;
                        }
                        cmd if cmd.starts_with("/mode ") => {
                            let arg = cmd.split_whitespace().nth(1).unwrap_or("");
                            if let Some(m) = PermissionMode::parse(arg) {
                                state.permission_mode = m;
                                policy.set_mode(m);
                                state.entries.push(TranscriptEntry::SystemInfo(format!(
                                    "已实时切换权限模式 → {}",
                                    m.label()
                                )));
                            } else {
                                state.entries.push(TranscriptEntry::SystemInfo(
                                    "用法: /mode auto | yolo | ask".to_string(),
                                ));
                            }
                            state.auto_scroll = true;
                            continue;
                        }
                        _ => {}
                    }

                    // 判断运行中是「立即打断」(Ctrl+Enter 或以 `!` 开头) 还是「默认排队」(直接 Enter)
                    let (interrupt_now, trimmed) = if let Some(rest) = raw_input.strip_prefix('!') {
                        let cleaned = rest.trim().to_string();
                        if cleaned.is_empty() {
                            continue;
                        }
                        (true, cleaned)
                    } else {
                        (key.modifiers.contains(KeyModifiers::CONTROL), raw_input)
                    };

                    let was_running = state.is_running();
                    if was_running && interrupt_now {
                        if let Some(c) = cancel_queue.front() {
                            c.cancel();
                        }
                        if let Some(approval) = state.pending_approval.take()
                            && let Some(broker) = &approval_broker
                        {
                            broker.resolve(&approval.call_id, ApprovalOutcome::Deny);
                        }
                        state.entries.push(TranscriptEntry::SystemInfo(
                            "⚡ 已打断当前执行，切入新指令…".to_string(),
                        ));
                    } else if was_running {
                        // 放入独立排队区显示，不往 entries 插入提示以免把正在流式输出的回复劈成两半
                        state.queued_inputs.push_back(trimmed.clone());
                    }

                    match trimmed.as_str() {
                        "/compact" => {
                            let cancel = CancellationToken::new();
                            cancel_queue.push_back(cancel.clone());
                            state.pending_tasks += 1;
                            if !was_running {
                                state.activity = Some(("正在压缩上下文…".to_string(), Instant::now()));
                            }
                            state.auto_scroll = true;
                            let _ = task_tx.send(WorkerTask::Compact {
                                turn_id: state.turn_counter,
                                cancel,
                            });
                        }
                        "/undo" => {
                            cancel_queue.push_back(CancellationToken::new());
                            state.pending_tasks += 1;
                            if !was_running {
                                state.activity = Some(("正在撤销上一轮…".to_string(), Instant::now()));
                            }
                            state.auto_scroll = true;
                            let _ = task_tx.send(WorkerTask::Undo);
                        }
                        "/turns" => {
                            cancel_queue.push_back(CancellationToken::new());
                            state.pending_tasks += 1;
                            state.auto_scroll = true;
                            let _ = task_tx.send(WorkerTask::ListTurns);
                        }
                        cmd if cmd.starts_with("/fork ") || cmd.starts_with("/rollback ") => {
                            let arg = cmd.split_whitespace().nth(1).unwrap_or("");
                            match arg.trim_start_matches('#').parse::<u64>() {
                                Ok(target_turn_id) => {
                                    cancel_queue.push_back(CancellationToken::new());
                                    state.pending_tasks += 1;
                                    if !was_running {
                                        state.activity =
                                            Some((format!("正在回滚至 Turn #{target_turn_id}…"), Instant::now()));
                                    }
                                    state.auto_scroll = true;
                                    let _ = task_tx.send(WorkerTask::Fork { target_turn_id });
                                }
                                Err(_) => {
                                    state.entries.push(TranscriptEntry::SystemInfo(
                                        "用法: /fork <轮次号>（先输入 /turns 查看可用轮次）".to_string(),
                                    ));
                                    state.auto_scroll = true;
                                }
                            }
                        }
                        _ => {
                            state.turn_counter += 1;
                            let cancel = CancellationToken::new();
                            cancel_queue.push_back(cancel.clone());
                            state.pending_tasks += 1;
                            if !was_running {
                                state.activity = Some(("等待模型响应…".to_string(), Instant::now()));
                                state.entries.push(TranscriptEntry::User(trimmed.clone()));
                            } else if interrupt_now {
                                state.entries.push(TranscriptEntry::User(trimmed.clone()));
                            }
                            state.auto_scroll = true;
                            let _ = task_tx.send(WorkerTask::Prompt {
                                turn_id: state.turn_counter,
                                prompt: trimmed,
                                cancel,
                            });
                        }
                    }
                }
                _ => {}
            }
            }
        }
    };

    disable_raw_mode()?;
    let _ = set_mouse_capture(terminal.backend_mut(), false);
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    res
}

/// 统一开关终端鼠标捕获：兼顾 Win32 Console API 与 VT ANSI 控制序列（禁用 `?1007l` 防止滚轮被转成上下箭头）。
fn set_mouse_capture<W: std::io::Write>(w: &mut W, enabled: bool) -> std::io::Result<()> {
    if enabled {
        execute!(w, EnableMouseCapture)?;
        // ?1007l 禁用滚轮→上下箭头转换；?1000h+?1002h+?1006h 启用点击/拖拽/滚轮 SGR 事件
        w.write_all(b"\x1b[?1007l\x1b[?1000h\x1b[?1002h\x1b[?1006h")?;
    } else {
        w.write_all(b"\x1b[?1006l\x1b[?1002l\x1b[?1000l")?;
        execute!(w, DisableMouseCapture)?;
    }
    w.flush()
}

fn draw_ui(frame: &mut ratatui::Frame<'_>, state: &mut TuiState) {
    let bottom_height = if let Some(approval) = &state.pending_approval {
        let diff_lines = approval.summary.lines().count().min(8);
        (diff_lines as u16 + 4).clamp(6, 12)
    } else {
        (state.input.line_count() as u16 + 2).clamp(3, 8)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(bottom_height),
        ])
        .split(frame.area());

    let total_prompt = state.last_usage.prompt_tokens.max(1);
    let cache_pct = (state.last_usage.cache_hit_tokens * 100) / total_prompt;
    let status = if state.is_running() {
        let queued = state.pending_tasks.saturating_sub(1);
        if queued > 0 {
            format!("运行中 (排队 {queued} 条)")
        } else {
            "运行中".to_string()
        }
    } else {
        "空闲".to_string()
    };
    let header_text = format!(
        " 模型: {} | 权限: {} (Shift+Tab) | 状态: {} | 缓存: {}% ({}/{}) | 工作区: {}",
        state.model_id,
        state.permission_mode.label(),
        status,
        cache_pct,
        state.last_usage.cache_hit_tokens,
        state.last_usage.prompt_tokens,
        state.workspace
    );
    let header = Paragraph::new(header_text)
        .block(Block::default().title(" gcode ").borders(Borders::ALL))
        .style(Style::default().fg(Color::Cyan));
    frame.render_widget(header, chunks[0]);

    let mut lines: Vec<Line<'_>> = Vec::new();
    let entries_len = state.entries.len();
    let mut i = 0;
    while i < entries_len {
        match &state.entries[i] {
            TranscriptEntry::User(text) => {
                if !lines.is_empty() {
                    lines.push(Line::raw(""));
                }
                lines.push(Line::from(Span::styled(
                    format!("> {text}"),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::raw(""));
                i += 1;
            }
            TranscriptEntry::Assistant(text) => {
                let mut md_lines = render_markdown_lines(text);
                if let Some(first) = md_lines.first_mut() {
                    first.spans.insert(
                        0,
                        Span::styled("● ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    );
                }
                lines.extend(md_lines);
                lines.push(Line::raw(""));
                i += 1;
            }
            TranscriptEntry::SystemInfo(info) => {
                lines.push(Line::from(Span::styled(
                    info.clone(),
                    Style::default().fg(Color::Blue),
                )));
                i += 1;
            }
            TranscriptEntry::Thinking(_) | TranscriptEntry::ToolResult { .. } => {
                // 收集一段连续的执行轨迹（Thinking + ToolResult）做统一视觉分组与智能折叠
                let start = i;
                while i < entries_len
                    && matches!(
                        state.entries[i],
                        TranscriptEntry::Thinking(_) | TranscriptEntry::ToolResult { .. }
                    )
                {
                    i += 1;
                }
                let group = &state.entries[start..i];
                let is_tail_group = i == entries_len && state.is_running();
                render_execution_group(
                    group,
                    state.show_thinking,
                    state.show_tools,
                    is_tail_group,
                    &mut lines,
                );
                lines.push(Line::raw(""));
            }
        }
    }

    if state.is_running() {
        const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame_ch = SPINNER[state.spinner_tick % SPINNER.len()];
        let (desc, elapsed_secs) = match &state.activity {
            Some((label, started)) => (label.as_str(), started.elapsed().as_secs()),
            None => ("正在处理中…", 0),
        };
        let queued = state.pending_tasks.saturating_sub(1);
        let queue_hint = if queued > 0 {
            format!(" | 队列待执行: {queued} 条")
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{frame_ch} {desc} "),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("({elapsed_secs}s{queue_hint})"),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        for (idx, q) in state.queued_inputs.iter().enumerate() {
            let one_line = q.lines().next().unwrap_or(q);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  ⏳ 排队 #{}: ", idx + 1),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    truncate_chars(one_line, 72),
                    Style::default().fg(Color::Gray),
                ),
            ]));
        }
    }

    let view_height = chunks[1].height.saturating_sub(2);
    let inner_width = chunks[1].width.saturating_sub(2).max(1) as usize;
    let total_visual_lines: usize = lines
        .iter()
        .map(|l| l.width().max(1).div_ceil(inner_width))
        .sum();
    let max_scroll = (total_visual_lines.min(u16::MAX as usize) as u16).saturating_sub(view_height);

    if state.auto_scroll {
        state.scroll_offset = max_scroll;
    } else {
        state.scroll_offset = state.scroll_offset.min(max_scroll);
        if state.scroll_offset == max_scroll {
            state.auto_scroll = true;
        }
    }

    state.transcript_inner = Rect {
        x: chunks[1].x + 1,
        y: chunks[1].y + 1,
        width: chunks[1].width.saturating_sub(2),
        height: chunks[1].height.saturating_sub(2),
    };

    let transcript = Paragraph::new(lines)
        .block(Block::default().title(" 对话与执行轨迹（左键拖拽自动复制 | 滚轮滚动） ").borders(Borders::ALL))
        .wrap(Wrap { trim: false })
        .scroll((state.scroll_offset, 0));
    frame.render_widget(transcript, chunks[1]);

    // 实时高亮鼠标左键拖拽选区
    if let Some((start, end)) = state.selection
        && start != end
    {
        highlight_selection(frame.buffer_mut(), state.transcript_inner, start, end);
    }

    if let Some(approval) = &state.pending_approval {
        let max_diff = bottom_height.saturating_sub(4) as usize;
        let mut panel_lines = render_diff_lines(&approval.summary, max_diff.max(2));
        panel_lines.push(Line::raw(""));
        panel_lines.push(Line::from(vec![
            Span::styled(" [Y / Enter] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw("允许本次   "),
            Span::styled("[A] ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(format!("记住并始终允许 `{}`   ", approval.tool_name)),
            Span::styled("[N / Esc] ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw("拒绝"),
        ]));

        let panel = Paragraph::new(panel_lines)
            .block(
                Block::default()
                    .title(format!(" 权限确认: {} ", approval.tool_name))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(panel, chunks[2]);
    } else {
        let input_title = if state.is_running() {
            " 运行中 (Enter 排队 | Ctrl+Enter 或 `!指令` 立即打断 | Shift+Enter 换行 | Esc 中断) "
        } else {
            " 输入指令 (Enter 发送 | Shift+Enter 换行 | 支持多行粘贴 | ↑/↓ 历史 | /help) "
        };
        let (cursor_row, cursor_col) = state.input.cursor_line_and_col();
        let inner_rows = chunks[2].height.saturating_sub(2).max(1);
        let input_scroll_y = cursor_row.saturating_sub(inner_rows.saturating_sub(1));

        let display_lines: Vec<Line<'_>> = state
            .input
            .text()
            .split('\n')
            .enumerate()
            .map(|(idx, line)| {
                let prefix = if idx == 0 { "> " } else { "  " };
                Line::raw(format!("{prefix}{line}"))
            })
            .collect();
        let input = Paragraph::new(display_lines)
            .block(Block::default().title(input_title).borders(Borders::ALL))
            .scroll((input_scroll_y, 0));
        frame.render_widget(input, chunks[2]);

        // 将终端硬件光标精准定位到多行输入中的当前编辑位置（+1 左边框，+2 "> "/"  " 前缀）
        let max_x = chunks[2].x + chunks[2].width.saturating_sub(2);
        let max_y = chunks[2].y + chunks[2].height.saturating_sub(2);
        let cursor_x = (chunks[2].x + 3 + cursor_col).min(max_x);
        let cursor_y = (chunks[2].y + 1 + cursor_row.saturating_sub(input_scroll_y)).min(max_y);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// 将 `ToolCall` 参数格式化为简洁可读的单行动作摘要（如 shell 命令、文件路径、搜索词）。
fn summarize_tool_call(tc: &ToolCall) -> String {
    match tc.name.as_str() {
        "shell" => tc
            .arguments
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| {
                let one_line = c.lines().next().unwrap_or(c).trim();
                truncate_chars(one_line, 72)
            })
            .unwrap_or_default(),
        "read_file" => {
            let path = tc.arguments.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            match (
                tc.arguments.get("start_line").and_then(|v| v.as_u64()),
                tc.arguments.get("limit").and_then(|v| v.as_u64()),
            ) {
                (Some(s), Some(l)) => format!("{path}:{s}-{}", s + l.saturating_sub(1)),
                (Some(s), None) => format!("{path}:{s}+"),
                _ => path.to_string(),
            }
        }
        "edit_file" | "write_file" => tc
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        "grep" => {
            let pat = tc.arguments.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            format!("\"{}\"", truncate_chars(pat, 48))
        }
        "glob" => tc
            .arguments
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("*")
            .to_string(),
        "spawn_agent" => tc
            .arguments
            .get("task")
            .and_then(|v| v.as_str())
            .map(|t| truncate_chars(t, 60))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// 渲染一段连续的思考与工具执行轨迹：
/// - 默认智能折叠（> 3 条工具调用时收拢为清爽的一行统计摘要，运行中仅实时展示最新 3 条）；
/// - 按 `Ctrl+O` 展开全部工具明细，按 `Tab` 展开思考过程。
fn render_execution_group(
    group: &[TranscriptEntry],
    show_thinking: bool,
    show_tools: bool,
    is_tail_running: bool,
    lines: &mut Vec<Line<'static>>,
) {
    let mut tool_items: Vec<(&str, Option<&str>, &str, bool)> = Vec::new();
    let mut total_thinking_chars = 0usize;

    for entry in group {
        match entry {
            TranscriptEntry::Thinking(t) => total_thinking_chars += t.chars().count(),
            TranscriptEntry::ToolResult {
                name,
                action,
                output,
                is_error,
            } => tool_items.push((name.as_str(), action.as_deref(), output.as_str(), *is_error)),
            _ => {}
        }
    }

    // 情况 1：用户展开了思考或工具明细，或者工具数量 <= 3，直接按树形缩进清晰展示
    if show_thinking || show_tools || tool_items.len() <= 3 {
        if !show_thinking && total_thinking_chars > 0 {
            lines.push(Line::from(Span::styled(
                format!("  💭 思考过程已折叠 ({total_thinking_chars} 字) — Tab 展开"),
                Style::default().fg(Color::DarkGray),
            )));
        }
        for entry in group {
            match entry {
                TranscriptEntry::Thinking(text) if show_thinking => {
                    lines.push(Line::from(Span::styled(
                        "  💭 思考过程 (Tab 折叠):",
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )));
                    for l in text.lines() {
                        lines.push(Line::from(Span::styled(
                            format!("  │ {l}"),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
                TranscriptEntry::ToolResult {
                    name,
                    action,
                    output,
                    is_error,
                } => {
                    render_single_tool_line(name, action.as_deref(), output, *is_error, lines);
                }
                _ => {}
            }
        }
        return;
    }

    // 情况 2：工具调用超过 3 条且处于折叠态
    let (collapsed_slice, visible_tail) = if is_tail_running {
        let split = tool_items.len().saturating_sub(3);
        (&tool_items[..split], &tool_items[split..])
    } else {
        (&tool_items[..], &tool_items[tool_items.len()..])
    };

    if !collapsed_slice.is_empty() {
        let summary = format_tool_group_summary(collapsed_slice, total_thinking_chars);
        lines.push(Line::from(Span::styled(
            format!("  ▸ {summary} — Ctrl+O 展开详情"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    for &(name, action, output, is_error) in visible_tail {
        render_single_tool_line(name, action, output, is_error, lines);
    }
}

fn format_tool_group_summary(
    items: &[(&str, Option<&str>, &str, bool)],
    thinking_chars: usize,
) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut errors = 0usize;
    for &(name, _, _, is_err) in items {
        *counts.entry(name).or_insert(0) += 1;
        if is_err {
            errors += 1;
        }
    }
    let breakdown: Vec<String> = counts
        .into_iter()
        .map(|(k, v)| format!("{k}×{v}"))
        .collect();
    let err_part = if errors > 0 {
        format!(" | {errors} 项失败")
    } else {
        String::new()
    };
    let think_part = if thinking_chars > 0 {
        format!("，思考 {thinking_chars} 字")
    } else {
        String::new()
    };
    format!(
        "已执行 {} 项工具（{}{err_part}）{think_part}",
        items.len(),
        breakdown.join(", ")
    )
}

fn render_single_tool_line(
    name: &str,
    action: Option<&str>,
    output: &str,
    is_error: bool,
    lines: &mut Vec<Line<'static>>,
) {
    let (mark, color) = if is_error {
        ("✗", Color::Red)
    } else {
        ("✓", Color::Green)
    };
    let detail = match (name, action) {
        ("shell", Some(cmd)) if !cmd.is_empty() => {
            let exit_tag = output.lines().next().unwrap_or("");
            format!("{cmd}  {exit_tag}")
        }
        (_, Some(act)) if !act.is_empty() => {
            let first = output.lines().next().unwrap_or("");
            if name == "edit_file" || name == "write_file" {
                truncate_chars(first, 80)
            } else {
                act.to_string()
            }
        }
        _ => truncate_chars(output.lines().next().unwrap_or("(空)"), 80),
    };

    lines.push(Line::from(vec![
        Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{mark} {:<10} ", name),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(detail, Style::default().fg(Color::Gray)),
    ]));

    // 工具失败时，在下一行以暗红缩进展示首行真实错误内容，避免只看到 [exit_code: 1]
    if is_error {
        let err_snippet = output
            .lines()
            .skip_while(|l| l.trim().is_empty() || l.starts_with("[exit_code:"))
            .find(|l| !l.trim().is_empty())
            .map(|l| truncate_chars(l.trim(), 90));
        if let Some(snippet) = err_snippet {
            lines.push(Line::from(vec![
                Span::styled("  │   ↳ ", Style::default().fg(Color::DarkGray)),
                Span::styled(snippet, Style::default().fg(Color::Red)),
            ]));
        }
    }
}

/// 将当前界面对话与工具轨迹导出为纯文本，供 `/copy all` 一键复制。
fn render_plain_transcript(entries: &[TranscriptEntry]) -> String {
    let mut out = Vec::new();
    for entry in entries {
        match entry {
            TranscriptEntry::User(s) => out.push(format!("> {s}")),
            TranscriptEntry::Assistant(s) => out.push(s.clone()),
            TranscriptEntry::ToolResult {
                name,
                action,
                output,
                is_error,
            } => {
                let mark = if *is_error { "✗" } else { "✓" };
                let detail = action
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| output.lines().next().unwrap_or("(空)"));
                out.push(format!("[{mark} {name}] {detail}"));
            }
            TranscriptEntry::Thinking(_) | TranscriptEntry::SystemInfo(_) => {}
        }
    }
    out.join("\n\n")
}

/// 跨平台将文本写入系统剪贴板（Windows 使用 `clip`，macOS 使用 `pbcopy`，Linux 使用 `xclip`）。
fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("powershell");
        c.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; [Console]::In.ReadToEnd() | Set-Clipboard",
        ]);
        c
    } else if cfg!(target_os = "macos") {
        Command::new("pbcopy")
    } else {
        let mut c = Command::new("xclip");
        c.args(["-selection", "clipboard"]);
        c
    };

    let mut child = cmd.stdin(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("剪贴板命令退出码: {status}");
    }
    Ok(())
}

/// 跨平台从系统剪贴板读取 UTF-8 文本，供 `Ctrl+V` 无损粘贴多行内容。
fn paste_from_clipboard() -> Result<String> {
    use std::process::Command;

    let out = if cfg!(windows) {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; Get-Clipboard -Raw",
            ])
            .output()?
    } else if cfg!(target_os = "macos") {
        Command::new("pbpaste").output()?
    } else {
        Command::new("xclip")
            .args(["-o", "-selection", "clipboard"])
            .output()?
    };

    if !out.status.success() {
        anyhow::bail!("读取剪贴板失败: {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn point_in_rect(x: u16, y: u16, r: Rect) -> bool {
    r.width > 0 && r.height > 0 && x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

fn clamp_x(x: u16, r: Rect) -> u16 {
    if r.width == 0 {
        r.x
    } else {
        x.clamp(r.x, r.x + r.width - 1)
    }
}

fn clamp_y(y: u16, r: Rect) -> u16 {
    if r.height == 0 {
        r.y
    } else {
        y.clamp(r.y, r.y + r.height - 1)
    }
}

fn normalize_selection(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if a.1 < b.1 || (a.1 == b.1 && a.0 <= b.0) {
        (a, b)
    } else {
        (b, a)
    }
}

fn highlight_selection(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    a: (u16, u16),
    b: (u16, u16),
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let ((sx, sy), (ex, ey)) = normalize_selection(a, b);
    let y_min = sy.max(area.y);
    let y_max = ey.min(area.y + area.height - 1);
    for y in y_min..=y_max {
        let x_start = if y == sy { sx.max(area.x) } else { area.x };
        let x_end = if y == ey {
            ex.min(area.x + area.width - 1)
        } else {
            area.x + area.width - 1
        };
        for x in x_start..=x_end {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_bg(Color::Blue).set_fg(Color::White);
            }
        }
    }
}

fn extract_selected_text(
    buf: &ratatui::buffer::Buffer,
    area: Rect,
    a: (u16, u16),
    b: (u16, u16),
) -> String {
    if area.width == 0 || area.height == 0 {
        return String::new();
    }
    let ((sx, sy), (ex, ey)) = normalize_selection(a, b);
    let y_min = sy.max(area.y);
    let y_max = ey.min(area.y + area.height - 1);
    let mut lines = Vec::new();
    for y in y_min..=y_max {
        let x_start = if y == sy { sx.max(area.x) } else { area.x };
        let x_end = if y == ey {
            ex.min(area.x + area.width - 1)
        } else {
            area.x + area.width - 1
        };
        let mut row_str = String::new();
        let mut x = x_start;
        while x <= x_end {
            if let Some(cell) = buf.cell((x, y)) {
                let sym = cell.symbol();
                if !sym.is_empty() {
                    row_str.push_str(sym);
                    let w = Line::raw(sym).width().max(1) as u16;
                    x = x.saturating_add(w);
                    continue;
                }
            }
            x += 1;
        }
        lines.push(row_str.trim_end().to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;

    #[test]
    fn extracts_selected_text_without_borders() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 4));
        buf.set_string(1, 1, "hello 世界", Style::default());
        buf.set_string(1, 2, "second line", Style::default());
        let area = Rect::new(1, 1, 18, 2);
        let text = extract_selected_text(&buf, area, (1, 1), (6, 2));
        assert_eq!(text, "hello 世界\nsecond");
    }
}
