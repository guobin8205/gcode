use async_trait::async_trait;
use gcode_protocol::ToolDefinition;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::fs::{build_object_schema, resolve_workspace_path};
use crate::registry::{Tool, ToolContext, ToolOutput};

/// 创建新文件或完整覆盖文件内容（自动创建缺失的父目录）。
pub struct WriteFileTool;

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".to_string(),
            description: "新建文件或完整覆写文件，必要时自动创建父目录。局部修改已有文件请优先使用 edit_file。".to_string(),
            parameters: build_object_schema(
                vec![
                    ("path", json!({ "type": "string", "description": "相对于工作区根目录的文件路径" })),
                    ("content", json!({ "type": "string", "description": "写入的完整文件内容" })),
                ],
                vec!["path", "content"],
            ),
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> ToolOutput {
        let parsed: WriteArgs = match serde_json::from_value(args) {
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

        if let Some(parent) = target.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolOutput {
                content: format!("创建父目录失败: {e}"),
                is_error: true,
            };
        }

        if let Err(e) = tokio::fs::write(&target, &parsed.content).await {
            return ToolOutput {
                content: format!("写入文件失败 {}: {e}", parsed.path),
                is_error: true,
            };
        }

        let lines = parsed.content.lines().count();
        ToolOutput {
            content: format!("已写入 {}（共 {lines} 行）", parsed.path),
            is_error: false,
        }
    }
}
