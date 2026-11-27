use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use gcode_protocol::{EventKind, Message, Role, SessionEvent};

/// Micro-Compaction 触发阈值（估算 Token 数）；未超过时保持前缀原样以最大化 Prefix Cache 命中。
pub const DEFAULT_MICRO_COMPACT_THRESHOLD: usize = 32_000;
/// 自动全量压缩触发阈值（估算 Token 数）。
pub const DEFAULT_AUTO_COMPACT_THRESHOLD: usize = 80_000;
/// Micro-Compaction 保留完整的最近工具输出条数。
pub const DEFAULT_KEEP_RECENT_TOOLS: usize = 4;
/// 早期工具输出折叠阈值（字符数）。
pub const DEFAULT_MAX_OLD_TOOL_CHARS: usize = 600;

/// 单次全量压缩的执行结果摘要。
#[derive(Debug, Clone)]
pub struct CompactionReport {
    pub compacted: bool,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub memory_saved: bool,
    pub reason: String,
}

/// 粗略估算单条文本的 Token 数（中文/非 ASCII 约 1.5 字/token，ASCII 约 4 字符/token）。
pub fn estimate_text_tokens(text: &str) -> usize {
    let mut ascii_chars = 0usize;
    let mut non_ascii_chars = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_chars += 1;
        } else {
            non_ascii_chars += 1;
        }
    }
    ascii_chars.div_ceil(4) + (non_ascii_chars * 2).div_ceil(3)
}

/// 估算一组对话消息的总 Token 数。
pub fn estimate_messages_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| {
            let mut total = 8 + estimate_text_tokens(&m.content);
            if let Some(r) = &m.reasoning_content {
                total += estimate_text_tokens(r);
            }
            for tc in &m.tool_calls {
                total += estimate_text_tokens(&tc.name);
                total += estimate_text_tokens(&tc.arguments.to_string());
            }
            total
        })
        .sum()
}

/// 第一阶段：Micro-Compaction（无损优先）。
/// 保留最近 `keep_recent_tool_results` 条 `Role::Tool` 的完整内容；
/// 对更早且超长的工具输出做首尾摘要折叠，显著降低上下文体积且不改动 JSONL 日志。
pub fn micro_compact_messages(
    mut messages: Vec<Message>,
    keep_recent_tool_results: usize,
    max_old_tool_chars: usize,
) -> Vec<Message> {
    let tool_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| (m.role == Role::Tool).then_some(i))
        .collect();

    if tool_indices.len() <= keep_recent_tool_results {
        return messages;
    }

    let cutoff = tool_indices.len() - keep_recent_tool_results;
    for &idx in &tool_indices[..cutoff] {
        let content = &messages[idx].content;
        let char_len = content.chars().count();
        if char_len > max_old_tool_chars {
            let head: String = content.chars().take(240).collect();
            let tail: String = content
                .chars()
                .rev()
                .take(140)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            messages[idx].content = format!(
                "[早期工具输出已折叠：原长 {char_len} 字符，如需完整内容可重新调用工具]\n{head}\n...\n{tail}"
            );
        }
    }

    messages
}

/// 根据事件流计算压缩切分点 `first_kept_seq`：
/// 保留最近 `keep_recent_turns` 个 `TurnStarted` 及其之后的所有事件；
/// 返回 `Some((first_kept_seq, compacted_events_slice))`，若可压缩历史不足则返回 `None`。
pub fn find_compaction_split<'a>(
    events: &'a [SessionEvent],
    keep_recent_turns: usize,
) -> Option<(u64, &'a [SessionEvent])> {
    // 若之前已做过压缩，只考虑上次压缩之后的活跃区间
    let active_start_idx = events
        .iter()
        .rposition(|e| matches!(e.kind, EventKind::CompactionPerformed { .. }))
        .map_or(0, |pos| pos + 1);

    let active = &events[active_start_idx..];
    let turn_starts: Vec<usize> = active
        .iter()
        .enumerate()
        .filter_map(|(i, e)| matches!(e.kind, EventKind::TurnStarted { .. }).then_some(i))
        .collect();

    if turn_starts.is_empty() {
        return None;
    }

    if keep_recent_turns == 0 {
        // 压掉全部活跃事件，切分点设为最后一条事件之后
        let next_seq = active.last()?.seq + 1;
        return Some((next_seq, active));
    }

    if turn_starts.len() <= keep_recent_turns {
        return None;
    }

    let split_idx = turn_starts[turn_starts.len() - keep_recent_turns];
    if split_idx == 0 {
        return None;
    }

    let first_kept_seq = active[split_idx].seq;
    Some((first_kept_seq, &active[..split_idx]))
}

/// 将待压缩的历史事件序列化为精简可读文本，供静默压缩提示词使用。
pub fn render_events_for_compaction(
    previous_summary: Option<&str>,
    events: &[SessionEvent],
) -> String {
    let mut out = String::new();
    if let Some(prev) = previous_summary
        && !prev.trim().is_empty()
    {
        out.push_str("【此前已有的历史摘要】\n");
        out.push_str(prev.trim());
        out.push_str("\n\n【新增待压缩对话记录】\n");
    }

    for ev in events {
        match &ev.kind {
            EventKind::TurnStarted { user_input, .. } => {
                out.push_str(&format!("\n[用户 Turn {}]\n{}\n", ev.turn_id, user_input));
            }
            EventKind::AssistantMessage {
                content,
                tool_calls,
                ..
            } => {
                if !content.trim().is_empty() {
                    let snippet: String = content.chars().take(1200).collect();
                    out.push_str(&format!("[助手回复]\n{snippet}\n"));
                }
                if !tool_calls.is_empty() {
                    let names: Vec<&str> = tool_calls.iter().map(|t| t.name.as_str()).collect();
                    out.push_str(&format!("[调用工具] {}\n", names.join(", ")));
                }
            }
            EventKind::ToolResult {
                tool_name,
                output,
                is_error,
                ..
            } => {
                let status = if *is_error { "失败" } else { "成功" };
                let snippet: String = output.chars().take(400).collect();
                out.push_str(&format!("[工具结果 {tool_name}({status})] {snippet}\n"));
            }
            _ => {}
        }
    }

    out
}

/// 构造 Silent Memory Flush + 对话压缩提示词。
pub fn build_compaction_prompt(transcript: &str) -> String {
    format!(
        "请对以下历史编程会话执行两项整理任务，并严格按两个二级标题输出：\n\n\
         ## 长期记忆\n\
         仅提取值得长期保存在项目 `.gcode/MEMORY.md` 中的关键事实：架构决策、项目约定、环境/依赖坑点、用户明确偏好。\n\
         如果没有值得长期保存的内容，请只写一个字：无\n\n\
         ## 对话摘要\n\
         用简洁的条目总结：\n\
         1. 用户的核心目标与约束\n\
         2. 已完成的工作及修改过的关键文件路径\n\
         3. 当前进度、未解决的问题与下一步待办\n\n\
         要求：只陈述实际发生过的事实，不编造未执行的改动，整体控制在 600 字以内。\n\n\
         ---\n\
         {transcript}"
    )
}

/// 解析模型返回的压缩结果，拆分为 `(可选长期记忆, 对话摘要)`。
pub fn parse_compaction_output(raw: &str) -> (Option<String>, String) {
    let trimmed = raw.trim();
    if let Some(summary_pos) = trimmed.find("## 对话摘要") {
        let memory_section = trimmed[..summary_pos]
            .replace("## 长期记忆", "")
            .trim()
            .to_string();
        let summary_section = trimmed[summary_pos + "## 对话摘要".len()..]
            .trim()
            .to_string();

        let memory = if memory_section.is_empty()
            || memory_section == "无"
            || memory_section == "无。"
            || memory_section.eq_ignore_ascii_case("none")
        {
            None
        } else {
            Some(memory_section)
        };

        let summary = if summary_section.is_empty() {
            trimmed.to_string()
        } else {
            summary_section
        };
        (memory, summary)
    } else {
        (None, trimmed.to_string())
    }
}

/// 将 Silent Memory Flush 提取出的长期记忆追加写入 `<workspace>/.gcode/MEMORY.md`。
pub fn append_memory_markdown(workspace_root: &Path, memory: &str) -> Result<()> {
    let gcode_dir = workspace_root.join(".gcode");
    fs::create_dir_all(&gcode_dir)?;
    let mem_path = gcode_dir.join("MEMORY.md");
    let is_new = !mem_path.exists() || fs::metadata(&mem_path).map_or(true, |m| m.len() == 0);

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&mem_path)?;

    if is_new {
        writeln!(file, "# gcode 项目长期记忆\n")?;
    }
    let ts = Utc::now().format("%Y-%m-%d %H:%M UTC");
    writeln!(file, "## 记录 ({ts})\n{}\n", memory.trim())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn micro_compaction_folds_old_large_tool_outputs_only() {
        let big = "A".repeat(1000);
        let mut msgs = Vec::new();
        for i in 0..6 {
            msgs.push(Message {
                role: Role::Tool,
                content: format!("tool-{i}-{big}"),
                reasoning_content: None,
                tool_calls: Vec::new(),
                tool_call_id: Some(format!("c{i}")),
            });
        }

        let compacted = micro_compact_messages(msgs, 2, 500);
        assert!(compacted[0].content.contains("早期工具输出已折叠"));
        assert!(compacted[3].content.contains("早期工具输出已折叠"));
        // 最近 2 条保持原样
        assert!(!compacted[4].content.contains("早期工具输出已折叠"));
        assert!(!compacted[5].content.contains("早期工具输出已折叠"));
    }

    #[test]
    fn parse_compaction_output_splits_memory_and_summary() {
        let raw = "## 长期记忆\n- 智谱使用 Coding 套餐端点\n\n## 对话摘要\n已完成配置改造。";
        let (mem, summary) = parse_compaction_output(raw);
        assert_eq!(mem.as_deref(), Some("- 智谱使用 Coding 套餐端点"));
        assert_eq!(summary, "已完成配置改造。");

        let raw_none = "## 长期记忆\n无\n\n## 对话摘要\n仅查询了目录结构。";
        let (mem2, summary2) = parse_compaction_output(raw_none);
        assert!(mem2.is_none());
        assert_eq!(summary2, "仅查询了目录结构。");
    }
}
