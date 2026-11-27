use std::fs;
use std::path::PathBuf;

use async_trait::async_trait;
use gcode_protocol::ToolDefinition;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::fs::build_object_schema;
use crate::registry::{Tool, ToolContext, ToolOutput};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    /// `pending` | `in_progress` | `completed` | `cancelled`
    pub status: String,
}

#[derive(Deserialize)]
struct TodoArgs {
    /// 操作类型：`read`（查看当前清单）或 `write`（整体更新清单）
    action: String,
    #[serde(default)]
    items: Vec<TodoItem>,
}

/// 任务进度追踪工具：在 `.gcode/todo.json` 维护结构化任务清单并返回可视化状态表。
pub struct TodoTool;

impl TodoTool {
    fn storage_path(workspace_root: &std::path::Path) -> PathBuf {
        workspace_root.join(".gcode").join("todo.json")
    }

    fn format_items(items: &[TodoItem]) -> String {
        if items.is_empty() {
            return "当前任务清单为空。".to_string();
        }
        let mut lines = Vec::with_capacity(items.len() + 1);
        let done = items.iter().filter(|i| i.status == "completed").count();
        lines.push(format!("任务进度: {done}/{}", items.len()));
        for item in items {
            let icon = match item.status.as_str() {
                "completed" => "[x]",
                "in_progress" => "[>]",
                "cancelled" => "[-]",
                _ => "[ ]",
            };
            lines.push(format!("{icon} ({}) {}", item.id, item.content));
        }
        lines.join("\n")
    }
}

#[async_trait]
impl Tool for TodoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "todo".to_string(),
            description: "读取或更新当前会话的任务进度清单（复杂多步任务时用于追踪计划与完成状态）。"
                .to_string(),
            parameters: build_object_schema(
                vec![
                    (
                        "action",
                        json!({
                            "type": "string",
                            "enum": ["read", "write"],
                            "description": "read 读取当前任务清单；write 用 items 列表整体覆盖更新"
                        }),
                    ),
                    (
                        "items",
                        json!({
                            "type": "array",
                            "description": "action=write 时必填，任务条目列表",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": { "type": "string" },
                                    "content": { "type": "string" },
                                    "status": {
                                        "type": "string",
                                        "enum": ["pending", "in_progress", "completed", "cancelled"]
                                    }
                                },
                                "required": ["id", "content", "status"]
                            }
                        }),
                    ),
                ],
                vec!["action"],
            ),
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> ToolOutput {
        let parsed: TodoArgs = match serde_json::from_value(args) {
            Ok(v) => v,
            Err(e) => {
                return ToolOutput {
                    content: format!("todo 参数解析失败: {e}"),
                    is_error: true,
                };
            }
        };

        let path = Self::storage_path(&ctx.workspace_root);
        match parsed.action.as_str() {
            "read" => {
                if !path.exists() {
                    return ToolOutput {
                        content: "当前任务清单为空。".to_string(),
                        is_error: false,
                    };
                }
                match fs::read_to_string(&path)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<Vec<TodoItem>>(&raw).ok())
                {
                    Some(items) => ToolOutput {
                        content: Self::format_items(&items),
                        is_error: false,
                    },
                    None => ToolOutput {
                        content: "读取 .gcode/todo.json 失败或格式损坏。".to_string(),
                        is_error: true,
                    },
                }
            }
            "write" => {
                if let Some(parent) = path.parent()
                    && let Err(e) = fs::create_dir_all(parent)
                {
                    return ToolOutput {
                        content: format!("创建 .gcode 目录失败: {e}"),
                        is_error: true,
                    };
                }
                match serde_json::to_string_pretty(&parsed.items) {
                    Ok(json_str) => {
                        if let Err(e) = fs::write(&path, json_str) {
                            return ToolOutput {
                                content: format!("写入 todo.json 失败: {e}"),
                                is_error: true,
                            };
                        }
                        ToolOutput {
                            content: Self::format_items(&parsed.items),
                            is_error: false,
                        }
                    }
                    Err(e) => ToolOutput {
                        content: format!("序列化 todo 失败: {e}"),
                        is_error: true,
                    },
                }
            }
            other => ToolOutput {
                content: format!("不支持的 action: {other}（仅支持 read 或 write）"),
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn todo_write_and_read_roundtrip() {
        let dir = std::env::temp_dir().join(format!("gcode_todo_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let tool = TodoTool;
        let ctx = ToolContext {
            workspace_root: dir.clone(),
            call_id: "t1".into(),
        };

        let w = tool
            .execute(
                ctx.clone(),
                json!({
                    "action": "write",
                    "items": [
                        { "id": "1", "content": "设计接口", "status": "completed" },
                        { "id": "2", "content": "编写单测", "status": "in_progress" }
                    ]
                }),
            )
            .await;
        assert!(!w.is_error);
        assert!(w.content.contains("任务进度: 1/2"));
        assert!(w.content.contains("[x] (1) 设计接口"));

        let r = tool.execute(ctx, json!({ "action": "read" })).await;
        assert!(!r.is_error);
        assert!(r.content.contains("[>] (2) 编写单测"));

        let _ = fs::remove_dir_all(&dir);
    }
}
