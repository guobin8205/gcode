use ratatui::text::Line;

/// 触发折叠为 `[Pasted text #N ...]` 块的单行字符数阈值（含换行符时一律折叠）。
const PASTE_BLOCK_CHAR_THRESHOLD: usize = 160;

/// 支持 UTF-8 中文光标、多行编辑、输入历史与 Claude Code 风格粘贴块折叠的输入组件。
#[derive(Debug, Default)]
pub struct InputState {
    buf: String,
    /// 按 Unicode 标量值（`char`）计数的光标位置，范围 `0..=buf.chars().count()`。
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    draft: String,
    /// 折叠的粘贴块：`(占位标签, 完整原始正文)`。
    pasted_blocks: Vec<(String, String)>,
    paste_seq: usize,
}

impl InputState {
    pub fn text(&self) -> &str {
        &self.buf
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn byte_index_of(&self, char_idx: usize) -> usize {
        self.buf
            .char_indices()
            .nth(char_idx)
            .map_or(self.buf.len(), |(b, _)| b)
    }

    pub fn insert_char(&mut self, c: char) {
        let byte_idx = self.byte_index_of(self.cursor);
        self.buf.insert(byte_idx, c);
        self.cursor += 1;
        self.history_idx = None;
    }

    /// 插入粘贴文本：
    /// - 多行文本或超长单行（>= 160 字）自动折叠为 `[Pasted text #N +M lines]` 块，退格一次可整体删除；
    /// - 短单行直接明文插入，方便粘贴文件名/变量名。
    pub fn insert_paste(&mut self, raw: &str) {
        let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
        let trimmed_end = normalized.trim_end_matches('\n');
        if trimmed_end.is_empty() {
            return;
        }

        let line_cnt = trimmed_end.split('\n').count();
        let char_cnt = trimmed_end.chars().count();
        if line_cnt > 1 || char_cnt >= PASTE_BLOCK_CHAR_THRESHOLD {
            self.paste_seq += 1;
            let label = if line_cnt > 1 {
                format!("[Pasted text #{} +{} lines]", self.paste_seq, line_cnt)
            } else {
                format!("[Pasted text #{} +{} chars]", self.paste_seq, char_cnt)
            };
            self.pasted_blocks
                .push((label.clone(), trimmed_end.to_string()));
            self.insert_raw_str(&label);
        } else {
            self.insert_raw_str(trimmed_end);
        }
    }

    /// 原样插入字符串（不做块折叠）。
    pub fn insert_str(&mut self, raw: &str) {
        let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.is_empty() {
            return;
        }
        self.insert_raw_str(&normalized);
    }

    fn insert_raw_str(&mut self, text: &str) {
        let byte_idx = self.byte_index_of(self.cursor);
        self.buf.insert_str(byte_idx, text);
        self.cursor += text.chars().count();
        self.history_idx = None;
    }

    pub fn line_count(&self) -> usize {
        self.buf.split('\n').count().max(1)
    }

    /// 退格删除：若光标紧贴某个 `[Pasted text #...]` 块末尾，一次整体删除该块。
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end_byte = self.byte_index_of(self.cursor);
        let prefix = &self.buf[..end_byte];
        if let Some((label_len_bytes, label_len_chars)) = self
            .pasted_blocks
            .iter()
            .find(|(label, _)| prefix.ends_with(label.as_str()))
            .map(|(label, _)| (label.len(), label.chars().count()))
        {
            let start_byte = end_byte - label_len_bytes;
            self.buf.replace_range(start_byte..end_byte, "");
            self.cursor = self.cursor.saturating_sub(label_len_chars);
            self.history_idx = None;
            return;
        }

        let start = self.byte_index_of(self.cursor - 1);
        self.buf.replace_range(start..end_byte, "");
        self.cursor -= 1;
        self.history_idx = None;
    }

    /// Delete 删除：若光标紧贴某个 `[Pasted text #...]` 块开头，一次整体删除该块。
    pub fn delete(&mut self) {
        let len = self.buf.chars().count();
        if self.cursor >= len {
            return;
        }
        let start_byte = self.byte_index_of(self.cursor);
        let suffix = &self.buf[start_byte..];
        if let Some(label_len_bytes) = self
            .pasted_blocks
            .iter()
            .find(|(label, _)| suffix.starts_with(label.as_str()))
            .map(|(label, _)| label.len())
        {
            self.buf
                .replace_range(start_byte..(start_byte + label_len_bytes), "");
            self.history_idx = None;
            return;
        }

        let end = self.byte_index_of(self.cursor + 1);
        self.buf.replace_range(start_byte..end, "");
        self.history_idx = None;
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        let len = self.buf.chars().count();
        if self.cursor < len {
            self.cursor += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buf.chars().count();
    }

    pub fn clear_to_start(&mut self) {
        let byte_idx = self.byte_index_of(self.cursor);
        self.buf.replace_range(..byte_idx, "");
        self.cursor = 0;
        self.history_idx = None;
    }

    pub fn clear_to_end(&mut self) {
        let byte_idx = self.byte_index_of(self.cursor);
        self.buf.truncate(byte_idx);
        self.history_idx = None;
    }

    /// 清空整个输入框与已暂存的粘贴块。
    pub fn clear_all(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.history_idx = None;
        self.pasted_blocks.clear();
    }

    /// 提交当前输入：展开所有 `[Pasted text #...]` 粘贴块为原始全文，并记入历史。
    pub fn take_and_record(&mut self) -> String {
        let mut taken = std::mem::take(&mut self.buf);
        let blocks = std::mem::take(&mut self.pasted_blocks);
        self.cursor = 0;
        self.history_idx = None;
        self.draft.clear();

        let display_trimmed = taken.trim().to_string();
        for (label, content) in blocks {
            taken = taken.replace(&label, &content);
        }
        let expanded = taken.trim().to_string();

        if !display_trimmed.is_empty()
            && self.history.last().map(String::as_str) != Some(expanded.as_str())
        {
            self.history.push(expanded.clone());
        }
        expanded
    }

    /// 向前翻阅历史（`↑` / `Ctrl+P`）。
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next_idx = match self.history_idx {
            None => {
                self.draft = self.buf.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_idx = Some(next_idx);
        self.buf = self.history[next_idx].clone();
        self.move_end();
    }

    /// 向后翻阅历史（`↓` / `Ctrl+N`），越过最新一条时恢复草稿。
    pub fn history_next(&mut self) {
        let Some(cur) = self.history_idx else {
            return;
        };
        if cur + 1 < self.history.len() {
            let next_idx = cur + 1;
            self.history_idx = Some(next_idx);
            self.buf = self.history[next_idx].clone();
        } else {
            self.history_idx = None;
            self.buf = std::mem::take(&mut self.draft);
        }
        self.move_end();
    }

    /// 计算光标前的终端视觉列宽（借助 Ratatui 内置 Unicode 宽度，准确兼容中文双宽字符）。
    pub fn cursor_visual_width(&self) -> u16 {
        self.cursor_line_and_col().1
    }

    /// 计算光标在多行输入中的 `(行号, 当前行列视觉宽度)`（均从 0 开始）。
    pub fn cursor_line_and_col(&self) -> (u16, u16) {
        let byte_idx = self.byte_index_of(self.cursor);
        let prefix = &self.buf[..byte_idx];
        let row = prefix.bytes().filter(|&b| b == b'\n').count().min(u16::MAX as usize) as u16;
        let current_line_prefix = prefix.rsplit('\n').next().unwrap_or(prefix);
        let col = Line::raw(current_line_prefix).width().min(u16::MAX as usize) as u16;
        (row, col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_paste_preserves_multiline_without_splitting() {
        let mut input = InputState::default();
        input.insert_str("line1\r\n第二行\rline3");
        assert_eq!(input.text(), "line1\n第二行\nline3");
        assert_eq!(input.line_count(), 3);
        assert_eq!(input.cursor_line_and_col(), (2, 5));
    }

    #[test]
    fn multiline_paste_collapses_into_atomic_block_and_expands_on_submit() {
        let mut input = InputState::default();
        input.insert_str("请修复：");
        input.insert_paste("err line 1\nerr line 2\nerr line 3\n");
        assert_eq!(input.text(), "请修复：[Pasted text #1 +3 lines]");
        assert_eq!(input.line_count(), 1);

        // 退格一次直接删掉整个粘贴块
        input.backspace();
        assert_eq!(input.text(), "请修复：");

        // 再粘贴一次并提交，确认自动展开为完整原文
        input.insert_paste("a\nb\nc");
        assert_eq!(input.text(), "请修复：[Pasted text #2 +3 lines]");
        let submitted = input.take_and_record();
        assert_eq!(submitted, "请修复：a\nb\nc");
    }

    #[test]
    fn handles_chinese_cursor_movement_and_edits() {
        let mut input = InputState::default();
        for c in "你好世界".chars() {
            input.insert_char(c);
        }
        assert_eq!(input.cursor_visual_width(), 8);

        input.move_left();
        input.move_left();
        input.insert_char('，');
        assert_eq!(input.text(), "你好，世界");

        input.backspace();
        assert_eq!(input.text(), "你好世界");

        input.delete();
        assert_eq!(input.text(), "你好界");

        input.move_home();
        input.clear_to_end();
        assert!(input.is_empty());
    }

    #[test]
    fn history_navigation_restores_draft() {
        let mut input = InputState::default();
        for c in "first".chars() {
            input.insert_char(c);
        }
        assert_eq!(input.take_and_record(), "first");

        for c in "second".chars() {
            input.insert_char(c);
        }
        assert_eq!(input.take_and_record(), "second");

        for c in "wip".chars() {
            input.insert_char(c);
        }
        input.history_prev();
        assert_eq!(input.text(), "second");
        input.history_prev();
        assert_eq!(input.text(), "first");
        input.history_next();
        assert_eq!(input.text(), "second");
        input.history_next();
        assert_eq!(input.text(), "wip");
    }
}
