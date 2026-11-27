pub mod fs;
pub mod hashline;
pub mod registry;
pub mod shell;
pub mod skills;
pub mod todo;

use std::sync::Arc;

pub use fs::edit::EditFileTool;
pub use fs::read::ReadFileTool;
pub use fs::search::{GlobTool, GrepTool};
pub use fs::write::WriteFileTool;
pub use hashline::{HashlineAnchor, HashlineError, apply_hashline_replace, format_with_hashlines};
pub use registry::{Tool, ToolContext, ToolOutput, ToolRegistry};
pub use shell::ShellTool;
pub use skills::{SkillMeta, discover_skills, format_skills_prompt};
pub use todo::{TodoItem, TodoTool};

/// 构建包含核心编码工具的默认确定性注册表（7 个常驻核心工具）。
pub fn default_core_registry() -> ToolRegistry {
    let mut reg = ToolRegistry::default();
    reg.register(Arc::new(ReadFileTool));
    reg.register(Arc::new(EditFileTool));
    reg.register(Arc::new(WriteFileTool));
    reg.register(Arc::new(GrepTool));
    reg.register(Arc::new(GlobTool));
    reg.register(Arc::new(ShellTool));
    reg.register(Arc::new(TodoTool));
    reg
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn core_tools_read_edit_and_grep_work_end_to_end() {
        let dir = std::env::temp_dir().join(format!("gcode_tools_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let reg = default_core_registry();
        let ctx = ToolContext {
            workspace_root: dir.clone(),
            call_id: "c1".to_string(),
        };

        let write_res = reg
            .get("write_file")
            .unwrap()
            .execute(ctx.clone(), json!({"path": "hello.rs", "content": "fn main() {\n    let x = 1;\n}\n"}))
            .await;
        assert!(!write_res.is_error);

        let read_res = reg
            .get("read_file")
            .unwrap()
            .execute(ctx.clone(), json!({"path": "hello.rs"}))
            .await;
        assert!(!read_res.is_error);
        let line2_anchor = read_res
            .content
            .lines()
            .find(|l| l.starts_with("2:"))
            .unwrap()
            .split('|')
            .next()
            .unwrap()
            .to_string();

        let edit_res = reg
            .get("edit_file")
            .unwrap()
            .execute(
                ctx.clone(),
                json!({"path": "hello.rs", "start_anchor": line2_anchor, "new_content": "    let x = 42;"}),
            )
            .await;
        assert!(!edit_res.is_error);

        let grep_res = reg
            .get("grep")
            .unwrap()
            .execute(ctx, json!({"pattern": "42"}))
            .await;
        assert!(!grep_res.is_error);
        assert!(grep_res.content.contains("hello.rs:2:"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
