use std::time::Duration;

use async_trait::async_trait;
use gcode_protocol::ToolDefinition;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::fs::build_object_schema;
use crate::registry::{Tool, ToolContext, ToolOutput};

const MAX_OUTPUT_CHARS: usize = 16_000;

/// 跨平台 Shell 命令执行工具，支持超时控制与输出截断。
pub struct ShellTool;

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    30
}

#[async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell".to_string(),
            description: "在工作区根目录执行终端命令（如构建、测试、git 状态查看），支持超时保护。".to_string(),
            parameters: build_object_schema(
                vec![
                    ("command", json!({ "type": "string", "description": "要执行的 Shell 命令" })),
                    ("timeout_secs", json!({ "type": "integer", "description": "超时秒数（默认 30，最大 300）" })),
                ],
                vec!["command"],
            ),
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> ToolOutput {
        let parsed: ShellArgs = match serde_json::from_value(args) {
            Ok(v) => v,
            Err(e) => {
                return ToolOutput {
                    content: format!("参数解析失败: {e}"),
                    is_error: true,
                };
            }
        };

        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command", &parsed.command]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", &parsed.command]);
            c
        };

        cmd.current_dir(&ctx.workspace_root).kill_on_drop(true);

        let timeout = Duration::from_secs(parsed.timeout_secs.clamp(1, 300));
        let output = match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                return ToolOutput {
                    content: format!("命令启动失败: {e}"),
                    is_error: true,
                };
            }
            Err(_) => {
                return ToolOutput {
                    content: format!("命令执行超时（>{} 秒）并已终止。", timeout.as_secs()),
                    is_error: true,
                };
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut combined = String::new();
        if !stdout.is_empty() {
            combined.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !combined.is_empty() {
                combined.push_str("\n[stderr]\n");
            }
            combined.push_str(&stderr);
        }
        if combined.is_empty() {
            combined.push_str("(无输出)");
        }

        if combined.chars().count() > MAX_OUTPUT_CHARS {
            let truncated: String = combined.chars().take(MAX_OUTPUT_CHARS).collect();
            combined = format!("{truncated}\n...（输出过长已截断）");
        }

        let code = output.status.code().unwrap_or(-1);
        ToolOutput {
            content: format!("[exit_code: {code}]\n{combined}"),
            is_error: !output.status.success(),
        }
    }
}
