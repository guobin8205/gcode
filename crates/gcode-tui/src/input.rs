use ratatui::text::Line;

/// 支持 UTF-8 中文字符光标移动与输入历史翻阅的命令行输入组件。
#[derive(Debug, Default)]
pub struct InputState {
    buf: String,
    /// 按 Unicode 标量值（`char`）计数的光标位置，范围 `0..=buf.chars().count()`。
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    draft: String,
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

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_index_of(self.cursor);
        let start = self.byte_index_of(self.cursor - 1);
        self.buf.replace_range(start..end, "");
        self.cursor -= 1;
        self.history_idx = None;
    }

    pub fn delete(&mut self) {
        let len = self.buf.chars().count();
        if self.cursor >= len {
            return;
        }
        let start = self.byte_index_of(self.cursor);
        let end = self.byte_index_of(self.cursor + 1);
        self.buf.replace_range(start..end, "");
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

    /// 提交当前输入并记入历史（自动去重连续相同条目）。
    pub fn take_and_record(&mut self) -> String {
        let taken = std::mem::take(&mut self.buf);
        self.cursor = 0;
        self.history_idx = None;
        self.draft.clear();

        let trimmed = taken.trim().to_string();
        if !trimmed.is_empty() && self.history.last().map(String::as_str) != Some(trimmed.as_str()) {
            self.history.push(trimmed.clone());
        }
        trimmed
    }

    /// 向前翻阅历史（`Ctrl+P`）。
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

    /// 向后翻阅历史（`Ctrl+N`），越过最新一条时恢复草稿。
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
        let byte_idx = self.byte_index_of(self.cursor);
        let prefix = &self.buf[..byte_idx];
        Line::raw(prefix).width().min(u16::MAX as usize) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
