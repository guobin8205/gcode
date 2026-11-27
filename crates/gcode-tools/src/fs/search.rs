use async_trait::async_trait;
use gcode_protocol::ToolDefinition;
use globset::Glob;
use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::fs::{build_object_schema, resolve_workspace_path};
use crate::registry::{Tool, ToolContext, ToolOutput};

/// 基于 `ignore` + `regex` 的内置 Ripgrep 风格文本搜索工具。
pub struct GrepTool;

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default = "default_dot")]
    path: String,
    glob: Option<String>,
}

fn default_dot() -> String {
    ".".to_string()
}

#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".to_string(),
            description: "在工作区内按正则表达式搜索代码内容（自动尊重 .gitignore），返回 `文件:行号: 内容`。".to_string(),
            parameters: build_object_schema(
                vec![
                    ("pattern", json!({ "type": "string", "description": "正则表达式或关键词" })),
                    ("path", json!({ "type": "string", "description": "可选搜索子目录，默认工作区根目录" })),
                    ("glob", json!({ "type": "string", "description": "可选文件名过滤通配符，例如 `*.rs`" })),
                ],
                vec!["pattern"],
            ),
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> ToolOutput {
        let parsed: GrepArgs = match serde_json::from_value(args) {
            Ok(v) => v,
            Err(e) => {
                return ToolOutput {
                    content: format!("参数解析失败: {e}"),
                    is_error: true,
                };
            }
        };

        let root = match resolve_workspace_path(&ctx.workspace_root, &parsed.path) {
            Ok(p) => p,
            Err(e) => {
                return ToolOutput {
                    content: e,
                    is_error: true,
                };
            }
        };

        let re = match Regex::new(&parsed.pattern) {
            Ok(r) => r,
            Err(e) => {
                return ToolOutput {
                    content: format!("无效正则表达式: {e}"),
                    is_error: true,
                };
            }
        };

        let glob_matcher = match &parsed.glob {
            Some(g) => match Glob::new(g) {
                Ok(compiled) => Some(compiled.compile_matcher()),
                Err(e) => {
                    return ToolOutput {
                        content: format!("无效 glob 模式: {e}"),
                        is_error: true,
                    };
                }
            },
            None => None,
        };

        let workspace_root = ctx.workspace_root.clone();
        let res = tokio::task::spawn_blocking(move || {
            let mut hits = Vec::new();
            for entry in WalkBuilder::new(&root).build().flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }
                let path = entry.path();
                if let Some(matcher) = &glob_matcher {
                    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or_default();
                    if !matcher.is_match(name) && !matcher.is_match(path) {
                        continue;
                    }
                }
                let Ok(text) = std::fs::read_to_string(path) else {
                    continue;
                };
                let rel = path
                    .strip_prefix(&workspace_root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");

                for (idx, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        hits.push(format!("{rel}:{}: {line}", idx + 1));
                        if hits.len() >= 100 {
                            hits.push("...（已达 100 条匹配上限，请缩小搜索范围）".to_string());
                            return hits;
                        }
                    }
                }
            }
            hits
        })
        .await;

        match res {
            Ok(hits) if hits.is_empty() => ToolOutput {
                content: "未找到匹配内容。".to_string(),
                is_error: false,
            },
            Ok(hits) => ToolOutput {
                content: hits.join("\n"),
                is_error: false,
            },
            Err(e) => ToolOutput {
                content: format!("搜索任务异常: {e}"),
                is_error: true,
            },
        }
    }
}

/// 基于 `ignore` + `globset` 的文件查找工具。
pub struct GlobTool;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default = "default_dot")]
    path: String,
}

#[async_trait]
impl Tool for GlobTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "glob".to_string(),
            description: "按通配符查找工作区内的文件路径（自动尊重 .gitignore）。".to_string(),
            parameters: build_object_schema(
                vec![
                    ("pattern", json!({ "type": "string", "description": "路径通配符，例如 `**/*.rs`" })),
                    ("path", json!({ "type": "string", "description": "可选起始子目录，默认工作区根目录" })),
                ],
                vec!["pattern"],
            ),
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> ToolOutput {
        let parsed: GlobArgs = match serde_json::from_value(args) {
            Ok(v) => v,
            Err(e) => {
                return ToolOutput {
                    content: format!("参数解析失败: {e}"),
                    is_error: true,
                };
            }
        };

        let root = match resolve_workspace_path(&ctx.workspace_root, &parsed.path) {
            Ok(p) => p,
            Err(e) => {
                return ToolOutput {
                    content: e,
                    is_error: true,
                };
            }
        };

        let matcher = match Glob::new(&parsed.pattern) {
            Ok(g) => g.compile_matcher(),
            Err(e) => {
                return ToolOutput {
                    content: format!("无效 glob 模式: {e}"),
                    is_error: true,
                };
            }
        };

        let workspace_root = ctx.workspace_root.clone();
        let res = tokio::task::spawn_blocking(move || {
            let mut files = Vec::new();
            for entry in WalkBuilder::new(&root).build().flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }
                let path = entry.path();
                let rel = path
                    .strip_prefix(&workspace_root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if matcher.is_match(&rel) {
                    files.push(rel);
                    if files.len() >= 200 {
                        files.push("...（已达 200 个文件上限）".to_string());
                        break;
                    }
                }
            }
            files
        })
        .await;

        match res {
            Ok(files) if files.is_empty() => ToolOutput {
                content: "未找到匹配文件。".to_string(),
                is_error: false,
            },
            Ok(files) => ToolOutput {
                content: files.join("\n"),
                is_error: false,
            },
            Err(e) => ToolOutput {
                content: format!("查找任务异常: {e}"),
                is_error: true,
            },
        }
    }
}
