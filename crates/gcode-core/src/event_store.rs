use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;
use gcode_protocol::{EventKind, Message, Role, SessionEvent};

/// 作为会话唯一事实源的只追加（Append-Only）JSONL 事件存储。
pub struct EventStore {
    log_path: PathBuf,
    events: Vec<SessionEvent>,
    next_seq: u64,
}

impl EventStore {
    pub fn open_or_create(log_path: impl AsRef<Path>) -> Result<Self> {
        let log_path = log_path.as_ref().to_path_buf();
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut events = Vec::new();
        if log_path.exists() {
            let file = fs::File::open(&log_path)?;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(ev) = serde_json::from_str::<SessionEvent>(&line) {
                    events.push(ev);
                }
            }
        }

        let next_seq = events.last().map_or(1, |e| e.seq + 1);
        Ok(Self {
            log_path,
            events,
            next_seq,
        })
    }

    pub fn append(&mut self, turn_id: u64, step_id: Option<u64>, kind: EventKind) -> Result<SessionEvent> {
        let event = SessionEvent {
            seq: self.next_seq,
            timestamp: Utc::now(),
            turn_id,
            step_id,
            ignorable: false,
            kind,
        };
        self.next_seq += 1;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        serde_json::to_writer(&mut file, &event)?;
        writeln!(file)?;

        self.events.push(event.clone());
        Ok(event)
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// 计算当前活跃事件流：按只追加语义顺序回放，遇到 `TurnForked { target_turn_id, .. }` 时
    /// 截断 `turn_id >= target_turn_id` 的历史分支，保留完整可审计底层日志。
    pub fn active_events(&self) -> Vec<SessionEvent> {
        let mut active: Vec<SessionEvent> = Vec::with_capacity(self.events.len());
        for ev in &self.events {
            if let EventKind::TurnForked { target_turn_id, .. } = ev.kind {
                active.retain(|e| e.turn_id < target_turn_id);
            } else {
                active.push(ev.clone());
            }
        }
        active
    }

    /// 列出当前活跃的所有 Turn：`(turn_id, 用户输入摘要, 是否记录了 Git 快照)`。
    pub fn active_turns(&self) -> Vec<(u64, String, bool)> {
        self.active_events()
            .into_iter()
            .filter_map(|ev| match ev.kind {
                EventKind::TurnStarted {
                    user_input,
                    git_tree_hash,
                } => Some((ev.turn_id, user_input, git_tree_hash.is_some())),
                _ => None,
            })
            .collect()
    }

    /// 查找指定活跃 Turn 启动时记录的 Git Tree 快照哈希。
    pub fn find_turn_snapshot(&self, target_turn_id: u64) -> Option<Option<String>> {
        self.active_events().into_iter().find_map(|ev| match ev.kind {
            EventKind::TurnStarted { git_tree_hash, .. } if ev.turn_id == target_turn_id => {
                Some(git_tree_hash)
            }
            _ => None,
        })
    }

    /// 返回当前事件流中最大的 `turn_id`（全局单调递增，避免分叉后 Turn ID 冲突）。
    pub fn last_turn_id(&self) -> u64 {
        self.events.iter().map(|e| e.turn_id).max().unwrap_or(0)
    }

    /// 查找工作区 `.gcode/sessions/` 下最近一次会话的 `(session_id, path)`。
    pub fn find_latest_session_path(workspace_root: &Path) -> Result<Option<(String, PathBuf)>> {
        let dir = workspace_root.join(".gcode").join("sessions");
        if !dir.exists() {
            return Ok(None);
        }
        let mut candidates: Vec<(String, PathBuf)> = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                candidates.push((stem.to_string(), path));
            }
        }
        candidates.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(candidates.pop())
    }

    /// 纯函数折叠（投影）：将活跃事件流转换为供 LLM 使用的对话消息列表。
    /// 若活跃事件流中存在 `CompactionPerformed`，则以最近一次压缩摘要为起点，仅投影 `seq >= first_kept_seq` 的事件。
    pub fn project_messages(&self) -> Vec<Message> {
        let active = self.active_events();
        let mut messages = Vec::new();
        let last_compaction = active.iter().rev().find_map(|ev| match &ev.kind {
            EventKind::CompactionPerformed {
                summary,
                first_kept_seq,
            } => Some((summary.as_str(), *first_kept_seq)),
            _ => None,
        });

        let min_seq = if let Some((summary, first_kept_seq)) = last_compaction {
            messages.push(Message {
                role: Role::User,
                content: format!("[历史对话压缩摘要]\n{summary}"),
                reasoning_content: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
            });
            first_kept_seq
        } else {
            0
        };

        let mut seen_assistant_with_tools = false;
        for ev in &active {
            if ev.seq < min_seq {
                continue;
            }
            match &ev.kind {
                EventKind::TurnStarted { user_input, .. } => {
                    seen_assistant_with_tools = false;
                    messages.push(Message {
                        role: Role::User,
                        content: user_input.clone(),
                        reasoning_content: None,
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                    });
                }
                EventKind::AssistantMessage {
                    content,
                    reasoning_content,
                    tool_calls,
                    ..
                } => {
                    seen_assistant_with_tools = !tool_calls.is_empty();
                    messages.push(Message {
                        role: Role::Assistant,
                        content: content.clone(),
                        reasoning_content: reasoning_content.clone(),
                        tool_calls: tool_calls.clone(),
                        tool_call_id: None,
                    });
                }
                EventKind::ToolResult {
                    call_id, output, ..
                } => {
                    // 防止压缩边界恰好截断在 tool_call 之后产生孤立 Tool 消息导致上游 400
                    if !seen_assistant_with_tools {
                        continue;
                    }
                    messages.push(Message {
                        role: Role::Tool,
                        content: output.clone(),
                        reasoning_content: None,
                        tool_calls: Vec::new(),
                        tool_call_id: Some(call_id.clone()),
                    });
                }
                _ => {}
            }
        }
        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gcode_protocol::TokenUsage;

    #[test]
    fn find_latest_session_and_resume_preserves_turns() {
        let dir = std::env::temp_dir().join(format!("gcode_resume_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sessions_dir = dir.join(".gcode").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let p1 = sessions_dir.join("20261126_100000.jsonl");
        let p2 = sessions_dir.join("20261126_120000.jsonl");
        let mut s1 = EventStore::open_or_create(&p1).unwrap();
        s1.append(1, None, EventKind::TurnStarted { user_input: "old".into(), git_tree_hash: None }).unwrap();

        let mut s2 = EventStore::open_or_create(&p2).unwrap();
        s2.append(1, None, EventKind::TurnStarted { user_input: "hi".into(), git_tree_hash: None }).unwrap();
        s2.append(
            1,
            Some(1),
            EventKind::AssistantMessage {
                content: "hello".into(),
                reasoning_content: None,
                tool_calls: Vec::new(),
                usage: TokenUsage::default(),
            },
        )
        .unwrap();

        let (latest_id, latest_path) = EventStore::find_latest_session_path(&dir).unwrap().unwrap();
        assert_eq!(latest_id, "20261126_120000");

        let reopened = EventStore::open_or_create(latest_path).unwrap();
        assert_eq!(reopened.last_turn_id(), 1);
        assert_eq!(reopened.project_messages().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_messages_respects_latest_compaction_boundary() {
        let dir = std::env::temp_dir().join(format!("gcode_compact_proj_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut store = EventStore::open_or_create(dir.join("session.jsonl")).unwrap();
        store
            .append(
                1,
                None,
                EventKind::TurnStarted {
                    user_input: "第一轮任务".into(),
                    git_tree_hash: None,
                },
            )
            .unwrap();
        store
            .append(
                1,
                Some(1),
                EventKind::AssistantMessage {
                    content: "第一轮回复".into(),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    usage: TokenUsage::default(),
                },
            )
            .unwrap();
        let t2 = store
            .append(
                2,
                None,
                EventKind::TurnStarted {
                    user_input: "第二轮任务".into(),
                    git_tree_hash: None,
                },
            )
            .unwrap();
        store
            .append(
                2,
                Some(1),
                EventKind::CompactionPerformed {
                    summary: "已总结第一轮任务。".into(),
                    first_kept_seq: t2.seq,
                },
            )
            .unwrap();

        let msgs = store.project_messages();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].content.contains("已总结第一轮任务"));
        assert_eq!(msgs[1].content, "第二轮任务");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
