use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// 将助手返回的 Markdown 文本渲染为带基础语法高亮的 Ratatui 行列表。
/// 支持：围栏代码块、标题、列表、引用块、行内代码与加粗文本。
pub fn render_markdown_lines(text: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_code_block = false;

    for raw_line in text.lines() {
        let trimmed = raw_line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            if in_code_block {
                in_code_block = false;
                out.push(Line::from(Span::styled(
                    "└────────────────────────────────────────",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                in_code_block = true;
                let lang = rest.trim();
                let header = if lang.is_empty() {
                    "┌─ code ─────────────────────────────────".to_string()
                } else {
                    format!("┌─ {lang} ─────────────────────────────────")
                };
                out.push(Line::from(Span::styled(
                    header,
                    Style::default().fg(Color::DarkGray),
                )));
            }
            continue;
        }

        if in_code_block {
            out.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(raw_line.to_string(), Style::default().fg(Color::LightCyan)),
            ]));
            continue;
        }

        // 标题行
        if let Some(h) = trimmed.strip_prefix("### ") {
            out.push(Line::from(Span::styled(
                h.to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("## ") {
            out.push(Line::from(Span::styled(
                h.to_string(),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("# ") {
            out.push(Line::from(Span::styled(
                h.to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
            continue;
        }

        // 引用块
        if let Some(quote) = trimmed.strip_prefix("> ") {
            let mut spans = vec![Span::styled("▎ ", Style::default().fg(Color::DarkGray))];
            spans.extend(parse_inline_spans(
                quote,
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::ITALIC),
            ));
            out.push(Line::from(spans));
            continue;
        }

        // 无序列表
        let indent_len = raw_line.len() - trimmed.len();
        let indent = &raw_line[..indent_len];
        if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let mut spans = vec![
                Span::raw(indent.to_string()),
                Span::styled("• ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            ];
            spans.extend(parse_inline_spans(item, Style::default()));
            out.push(Line::from(spans));
            continue;
        }

        // 普通段落（支持行内代码与加粗）
        out.push(Line::from(parse_inline_spans(raw_line, Style::default())));
    }

    out
}

/// 将包含 `+` / `-` / `@@` 前缀的 Diff 预览文本渲染为彩色行（最多显示 `max_lines` 行）。
pub fn render_diff_lines(text: &str, max_lines: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let total = text.lines().count();
    for raw in text.lines().take(max_lines) {
        let line = if raw.starts_with("@@") {
            Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ))
        } else if raw.starts_with("+ ") || (raw.starts_with('+') && !raw.starts_with("+++")) {
            Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(Color::Green),
            ))
        } else if raw.starts_with("- ") || (raw.starts_with('-') && !raw.starts_with("---")) {
            Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(Color::Red),
            ))
        } else {
            Line::raw(raw.to_string())
        };
        out.push(line);
    }
    if total > max_lines {
        out.push(Line::from(Span::styled(
            format!("...（其余 {} 行已省略）", total - max_lines),
            Style::default().fg(Color::DarkGray),
        )));
    }
    out
}

/// 解析单行内的 `` `code` `` 与 `**bold**` 语法。
fn parse_inline_spans(line: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>, style: Style| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), style));
        }
    };

    while i < chars.len() {
        // 行内代码 `...`
        if chars[i] == '`'
            && let Some(end_rel) = chars[i + 1..].iter().position(|&c| c == '`')
        {
            flush(&mut buf, &mut spans, base);
            let code: String = chars[i + 1..i + 1 + end_rel].iter().collect();
            spans.push(Span::styled(
                code,
                Style::default().fg(Color::LightYellow).bg(Color::DarkGray),
            ));
            i += end_rel + 2;
            continue;
        }

        // 加粗 **...**
        if i + 1 < chars.len()
            && chars[i] == '*'
            && chars[i + 1] == '*'
            && let Some(end_rel) = find_double_star(&chars[i + 2..])
        {
            flush(&mut buf, &mut spans, base);
            let bold_text: String = chars[i + 2..i + 2 + end_rel].iter().collect();
            spans.push(Span::styled(
                bold_text,
                base.add_modifier(Modifier::BOLD),
            ));
            i += end_rel + 4;
            continue;
        }

        buf.push(chars[i]);
        i += 1;
    }

    flush(&mut buf, &mut spans, base);
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

fn find_double_star(slice: &[char]) -> Option<usize> {
    (0..slice.len().saturating_sub(1)).find(|&i| slice[i] == '*' && slice[i + 1] == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_code_blocks_headings_and_inline_styles() {
        let md = "## 标题\n这是 **加粗** 和 `code`。\n```rust\nfn main() {}\n```";
        let lines = render_markdown_lines(md);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].spans[0].content, "标题");
        assert!(lines[1].spans.iter().any(|s| s.content == "加粗"));
        assert!(lines[1].spans.iter().any(|s| s.content == "code"));
        assert!(lines[3].spans.iter().any(|s| s.content == "fn main() {}"));
    }

    #[test]
    fn renders_colored_diff_lines_with_truncation() {
        let diff = "修改文件: a.rs\n- old line\n+ new line\n+ extra";
        let lines = render_diff_lines(diff, 3);
        assert_eq!(lines.len(), 4); // 3 行 + 1 行省略提示
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[2].spans[0].style.fg, Some(Color::Green));
    }
}
