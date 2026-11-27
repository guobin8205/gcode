use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// 发现的 `SKILL.md` 元数据索引；常驻前缀仅注入名称、一句话描述与路径，模型按需 `read_file` 加载全文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// 扫描工作区 `.gcode/skills/*/SKILL.md` 及用户全局 `~/.gcode/skills/*/SKILL.md`。
/// 若存在同名技能，工作区级覆盖全局级；结果按名称字典序稳定排序以保障 Prefix Cache。
pub fn discover_skills(workspace_root: &Path) -> Vec<SkillMeta> {
    let mut map: BTreeMap<String, SkillMeta> = BTreeMap::new();

    // 1. 先加载全局技能
    if let Some(home) = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
    {
        scan_skills_dir(&home.join(".gcode").join("skills"), &mut map);
    }

    // 2. 再加载工作区技能（同名覆盖全局）
    scan_skills_dir(&workspace_root.join(".gcode").join("skills"), &mut map);

    map.into_values().collect()
}

/// 将发现的技能索引格式化为字节级稳定的系统提示词片段。
pub fn format_skills_prompt(skills: &[SkillMeta]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut lines = vec![
        "## 可用技能 (Skills)".to_string(),
        "以下领域技能采用按需加载模式：当任务匹配某项技能时，先用 `read_file` 读取其 `SKILL.md` 完整指令后再执行。".to_string(),
    ];
    for s in skills {
        lines.push(format!(
            "- `{}` ({}): {}",
            s.name,
            s.path.display(),
            s.description
        ));
    }
    Some(lines.join("\n"))
}

fn scan_skills_dir(dir: &Path, out: &mut BTreeMap<String, SkillMeta>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_file = path.join("SKILL.md");
        if !skill_file.is_file() {
            continue;
        }
        let Ok(content) = fs::read_to_string(&skill_file) else {
            continue;
        };
        let fallback_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_string();
        let (name, description) = parse_skill_frontmatter(&content, &fallback_name);
        out.insert(
            name.clone(),
            SkillMeta {
                name,
                description,
                path: skill_file,
            },
        );
    }
}

/// 轻量解析 YAML Frontmatter 中的 `name` 与 `description`，无 Frontmatter 时降级取目录名与首行非空正文。
fn parse_skill_frontmatter(content: &str, fallback_name: &str) -> (String, String) {
    let mut name = fallback_name.to_string();
    let mut description = String::new();

    let trimmed = content.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---")
        && let Some(end_idx) = rest.find("\n---")
    {
        let front = &rest[..end_idx];
        for line in front.lines() {
            let l = line.trim();
            if let Some(val) = l.strip_prefix("name:") {
                let v = val.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    name = v.to_string();
                }
            } else if let Some(val) = l.strip_prefix("description:") {
                let v = val.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    description = v.to_string();
                }
            }
        }
    }

    if description.is_empty() {
        description = content
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && *l != "---" && !l.starts_with('#'))
            .unwrap_or("按需读取 SKILL.md 获取详细指令")
            .chars()
            .take(120)
            .collect();
    }

    (name, description)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_workspace_skills_and_parses_frontmatter() {
        let dir = std::env::temp_dir().join(format!("gcode_skills_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let skill_dir = dir.join(".gcode").join("skills").join("rust-refactor");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: rust-refactor\ndescription: 安全重构 Rust 代码并跑 clippy\n---\n\n# 详细步骤\n1. 先跑测试\n",
        )
        .unwrap();

        let skills = discover_skills(&dir);
        let found = skills.iter().find(|s| s.name == "rust-refactor").unwrap();
        assert_eq!(found.description, "安全重构 Rust 代码并跑 clippy");

        let prompt = format_skills_prompt(&skills).unwrap();
        assert!(prompt.contains("`rust-refactor`"));
        assert!(prompt.contains("安全重构 Rust 代码并跑 clippy"));

        let _ = fs::remove_dir_all(&dir);
    }
}
