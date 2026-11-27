use async_trait::async_trait;
use gcode_protocol::ToolDefinition;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::fs::{build_object_schema, resolve_workspace_path};
use crate::hashline::{HashlineAnchor, apply_hashline_replace, format_with_hashlines};
use crate::registry::{Tool, ToolContext, ToolOutput};

/// 代码编辑工具：优先使用 Hashline 行哈希锚点替换，同时支持 `old_string`/`new_string` 兜底。
pub struct EditFileTool;

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    start_anchor: Option<String>,
    end_anchor: Option<String>,
    new_content: Option<String>,
    old_string: Option<String>,
    new_string: Option<String>,
}

#[async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit_file".to_string(),
            description: "编辑文件。优先使用 read_file 返回的 Hashline 锚点（start_anchor/end_anchor + new_content）；也支持 old_string/new_string 精确替换。".to_string(),
            parameters: build_object_schema(
                vec![
                    ("path", json!({ "type": "string", "description": "相对于工作区根目录的文件路径" })),
                    ("start_anchor", json!({ "type": "string", "description": "起始行锚点，格式 `<行号>:<4位哈希>`" })),
                    ("end_anchor", json!({ "type": "string", "description": "可选结束行锚点，省略则只替换单行" })),
                    ("new_content", json!({ "type": "string", "description": "替换后的新代码内容（传空字符串可删除指定行）" })),
                    ("old_string", json!({ "type": "string", "description": "备选模式：待替换的原文片段" })),
                    ("new_string", json!({ "type": "string", "description": "备选模式：替换后的新文本" })),
                ],
                vec!["path"],
            ),
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> ToolOutput {
        let parsed: EditArgs = match serde_json::from_value(args) {
            Ok(v) => v,
            Err(e) => {
                return ToolOutput {
                    content: format!("参数解析失败: {e}"),
                    is_error: true,
                };
            }
        };

        let target = match resolve_workspace_path(&ctx.workspace_root, &parsed.path) {
            Ok(p) => p,
            Err(e) => {
                return ToolOutput {
                    content: e,
                    is_error: true,
                };
            }
        };

        let current = match tokio::fs::read_to_string(&target).await {
            Ok(s) => s,
            Err(e) => {
                return ToolOutput {
                    content: format!("读取目标文件失败 {}: {e}", parsed.path),
                    is_error: true,
                };
            }
        };

        let updated = if let Some(start_raw) = &parsed.start_anchor {
            let start = match HashlineAnchor::parse(start_raw) {
                Ok(a) => a,
                Err(e) => {
                    return ToolOutput {
                        content: e.to_string(),
                        is_error: true,
                    };
                }
            };
            let end = match &parsed.end_anchor {
                Some(raw) => match HashlineAnchor::parse(raw) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        return ToolOutput {
                            content: e.to_string(),
                            is_error: true,
                        };
                    }
                },
                None => None,
            };
            let replacement = parsed.new_content.as_deref().unwrap_or("");
            match apply_hashline_replace(&current, &start, end.as_ref(), replacement) {
                Ok(next) => next,
                Err(e) => {
                    let preview = preview_around_line(&current, start.line, 3);
                    return ToolOutput {
                        content: format!("{e}\n最新周边行如下，请核对锚点后重试：\n{preview}"),
                        is_error: true,
                    };
                }
            }
        } else if let (Some(old), Some(new)) = (&parsed.old_string, &parsed.new_string) {
            let count = current.matches(old).count();
            if count == 0 {
                return ToolOutput {
                    content: "old_string 未在文件中匹配到，建议改用 read_file + Hashline 锚点编辑。".to_string(),
                    is_error: true,
                };
            }
            if count > 1 {
                return ToolOutput {
                    content: format!("old_string 匹配到 {count} 处，请提供更精确上下文或改用 Hashline 锚点。"),
                    is_error: true,
                };
            }
            current.replacen(old, new, 1)
        } else {
            return ToolOutput {
                content: "必须提供 start_anchor（Hashline 模式）或 old_string + new_string。".to_string(),
                is_error: true,
            };
        };

        if let Err(e) = tokio::fs::write(&target, &updated).await {
            return ToolOutput {
                content: format!("写入文件失败 {}: {e}", parsed.path),
                is_error: true,
            };
        }

        let old_lines = current.lines().count();
        let new_lines = updated.lines().count();
        let diff_hint = if new_lines >= old_lines {
            format!("+{} 行", new_lines - old_lines)
        } else {
            format!("-{} 行", old_lines - new_lines)
        };

        ToolOutput {
            content: format!("已成功更新 {}（总行数 {} → {}，{}）", parsed.path, old_lines, new_lines, diff_hint),
            is_error: false,
        }
    }
}

fn preview_around_line(content: &str, center_line: usize, radius: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let start = center_line.saturating_sub(radius).max(1);
    let end = (center_line + radius).min(lines.len());
    let slice = lines[(start - 1)..end].join("\n");
    format_with_hashlines(&slice, start)
}
