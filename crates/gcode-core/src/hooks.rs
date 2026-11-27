use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const HOOK_TIMEOUT_SECS: u64 = 15;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub pre_tool_use: Vec<HookRule>,
    #[serde(default)]
    pub post_tool_use: Vec<HookRule>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookRule {
    /// 匹配规则：省略或 `"*"` 匹配全部工具；支持 `|` 分隔多个工具名（如 `"edit_file|write_file"`）。
    #[serde(default)]
    pub matcher: Option<String>,
    /// 执行的外部脚本或命令；事件负载以 JSON 形式写入其 `stdin`。
    pub command: String,
}

impl HookRule {
    fn matches(&self, tool_name: &str) -> bool {
        let Some(m) = self.matcher.as_deref() else {
            return true;
        };
        let trimmed = m.trim();
        if trimmed.is_empty() || trimmed == "*" {
            return true;
        }
        trimmed.split('|').any(|part| part.trim() == tool_name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreHookDecision {
    Allow,
    Block(String),
}

#[derive(Deserialize)]
struct HookJsonResponse {
    decision: Option<String>,
    reason: Option<String>,
}

/// 生命周期钩子执行器：加载 `<workspace>/.gcode/hooks.json` 并在工具执行前后触发外部脚本。
#[derive(Debug, Clone, Default)]
pub struct HookRunner {
    config: HooksConfig,
}

impl HookRunner {
    pub fn load_from_workspace(workspace_root: &Path) -> Self {
        let path = workspace_root.join(".gcode").join("hooks.json");
        let config = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<HooksConfig>(&raw).ok())
            .unwrap_or_default();
        Self { config }
    }

    pub fn from_config(config: HooksConfig) -> Self {
        Self { config }
    }

    /// 执行所有匹配的 `PreToolUse` 钩子；若任一钩子返回非零退出码或 `{"decision":"block"}`，则拦截调用。
    pub async fn run_pre_tool_use(
        &self,
        workspace_root: &Path,
        tool_name: &str,
        arguments: &Value,
    ) -> PreHookDecision {
        let payload = json!({
            "hook_event": "PreToolUse",
            "tool_name": tool_name,
            "arguments": arguments,
            "workspace_root": workspace_root.display().to_string(),
        });

        for rule in &self.config.pre_tool_use {
            if !rule.matches(tool_name) {
                continue;
            }
            match exec_hook_command(workspace_root, &rule.command, &payload).await {
                Ok((exit_code, stdout, stderr)) => {
                    if let Ok(parsed) = serde_json::from_str::<HookJsonResponse>(stdout.trim())
                        && parsed
                            .decision
                            .as_deref()
                            .is_some_and(|d| d.eq_ignore_ascii_case("block") || d.eq_ignore_ascii_case("deny"))
                    {
                        let reason = parsed
                            .reason
                            .unwrap_or_else(|| "被 PreToolUse Hook 拦截".to_string());
                        return PreHookDecision::Block(reason);
                    }
                    if exit_code != 0 {
                        let detail = if !stderr.trim().is_empty() {
                            stderr.trim().to_string()
                        } else if !stdout.trim().is_empty() {
                            stdout.trim().to_string()
                        } else {
                            format!("退出码 {exit_code}")
                        };
                        return PreHookDecision::Block(format!("PreToolUse Hook 拒绝执行: {detail}"));
                    }
                }
                Err(err) => {
                    return PreHookDecision::Block(format!("PreToolUse Hook 执行失败: {err}"));
                }
            }
        }

        PreHookDecision::Allow
    }

    /// 执行所有匹配的 `PostToolUse` 钩子；收集其非空输出作为附加反馈（如自动格式化/Lint 结果）。
    pub async fn run_post_tool_use(
        &self,
        workspace_root: &Path,
        tool_name: &str,
        arguments: &Value,
        output: &str,
        is_error: bool,
    ) -> Option<String> {
        let payload = json!({
            "hook_event": "PostToolUse",
            "tool_name": tool_name,
            "arguments": arguments,
            "output": output,
            "is_error": is_error,
            "workspace_root": workspace_root.display().to_string(),
        });

        let mut notes = Vec::new();
        for rule in &self.config.post_tool_use {
            if !rule.matches(tool_name) {
                continue;
            }
            match exec_hook_command(workspace_root, &rule.command, &payload).await {
                Ok((code, stdout, stderr)) => {
                    let msg = if !stdout.trim().is_empty() {
                        stdout.trim().to_string()
                    } else if code != 0 && !stderr.trim().is_empty() {
                        stderr.trim().to_string()
                    } else {
                        String::new()
                    };
                    if !msg.is_empty() {
                        notes.push(format!("[PostToolUse Hook] {msg}"));
                    }
                }
                Err(err) => {
                    notes.push(format!("[PostToolUse Hook 警告] {err}"));
                }
            }
        }

        if notes.is_empty() {
            None
        } else {
            Some(notes.join("\n"))
        }
    }
}

async fn exec_hook_command(
    workspace_root: &Path,
    command: &str,
    payload: &Value,
) -> Result<(i32, String, String), String> {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };

    cmd.current_dir(workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("启动进程失败: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = serde_json::to_vec(payload).unwrap_or_default();
        let _ = stdin.write_all(&bytes).await;
        drop(stdin);
    }

    let output = tokio::time::timeout(
        Duration::from_secs(HOOK_TIMEOUT_SECS),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| format!("Hook 执行超过 {HOOK_TIMEOUT_SECS}s 超时"))?
    .map_err(|e| format!("等待 Hook 进程失败: {e}"))?;

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Ok((code, stdout, stderr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pre_hook_blocks_matching_tool_and_allows_others() {
        let dir = std::env::temp_dir().join(format!("gcode_hook_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".gcode")).unwrap();

        let block_cmd = if cfg!(windows) {
            r#"Write-Output '{"decision":"block","reason":"禁止执行危险命令"}'"#
        } else {
            r#"printf '{"decision":"block","reason":"禁止执行危险命令"}'"#
        };

        fs::write(
            dir.join(".gcode").join("hooks.json"),
            serde_json::to_string(&json!({
                "pre_tool_use": [{
                    "matcher": "shell",
                    "command": block_cmd
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let runner = HookRunner::load_from_workspace(&dir);
        let res_shell = runner
            .run_pre_tool_use(&dir, "shell", &json!({"command": "rm -rf /"}))
            .await;
        assert_eq!(
            res_shell,
            PreHookDecision::Block("禁止执行危险命令".to_string())
        );

        let res_read = runner
            .run_pre_tool_use(&dir, "read_file", &json!({"path": "a.rs"}))
            .await;
        assert_eq!(res_read, PreHookDecision::Allow);

        let _ = fs::remove_dir_all(&dir);
    }
}
