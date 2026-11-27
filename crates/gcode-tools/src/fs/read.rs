use async_trait::async_trait;
use gcode_protocol::ToolDefinition;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::fs::{build_object_schema, resolve_readable_path};
use crate::hashline::format_with_hashlines;
use crate::registry::{Tool, ToolContext, ToolOutput};

/// 读取文件内容并自动附加 `行号:哈希|` 前缀，供后续 Hashline 编辑精确锚定。
pub struct ReadFileTool;

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default = "default_start_line")]
    start_line: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_start_line() -> usize {
    1
}

fn default_limit() -> usize {
    200
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".to_string(),
            description: "读取工作区内文件指定行范围，并在每行前输出 `<行号>:<4位哈希>|` 锚点供 edit_file 使用。".to_string(),
            parameters: build_object_schema(
                vec![
                    ("path", json!({ "type": "string", "description": "相对于工作区根目录的文件路径" })),
                    ("start_line", json!({ "type": "integer", "description": "起始行号（从 1 开始，默认 1）" })),
                    ("limit", json!({ "type": "integer", "description": "最多读取行数（默认 200）" })),
                ],
                vec!["path"],
            ),
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> ToolOutput {
        let parsed: ReadArgs = match serde_json::from_value(args) {
            Ok(v) => v,
            Err(e) => {
                return ToolOutput {
                    content: format!("参数解析失败: {e}"),
                    is_error: true,
                };
            }
        };

        let target = match resolve_readable_path(&ctx.workspace_root, &parsed.path) {
            Ok(p) => p,
            Err(e) => {
                return ToolOutput {
                    content: e,
                    is_error: true,
                };
            }
        };

        let raw = match tokio::fs::read_to_string(&target).await {
            Ok(s) => s,
            Err(e) => {
                return ToolOutput {
                    content: format!("读取文件失败 {}: {e}", parsed.path),
                    is_error: true,
                };
            }
        };

        let lines: Vec<&str> = raw.lines().collect();
        let total = lines.len();
        if total == 0 {
            return ToolOutput {
                content: format!("[{} | 空文件 (0 行)]", parsed.path),
                is_error: false,
            };
        }

        let start = parsed.start_line.max(1);
        if start > total {
            return ToolOutput {
                content: format!("起始行 {start} 超出文件总行数 {total}"),
                is_error: true,
            };
        }

        let limit = parsed.limit.clamp(1, 1000);
        let end = (start - 1 + limit).min(total);
        let slice = lines[(start - 1)..end].join("\n");
        let body = format_with_hashlines(&slice, start);

        ToolOutput {
            content: format!("[{} | 第 {start}-{end} 行 / 共 {total} 行]\n{body}", parsed.path),
            is_error: false,
        }
    }
}
