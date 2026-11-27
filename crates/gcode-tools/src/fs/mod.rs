pub mod edit;
pub mod read;
pub mod search;
pub mod write;

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

/// 将相对或绝对路径安全解析到工作区根目录内，防止路径穿越越界。
pub fn resolve_workspace_path(workspace_root: &Path, raw_path: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(raw_path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace_root.join(candidate)
    };

    let root_norm = normalize_lexical(workspace_root);
    let target_norm = normalize_lexical(&joined);
    if !target_norm.starts_with(&root_norm) {
        return Err(format!(
            "路径越界拒绝访问：{raw_path} 不在工作区 {} 内",
            workspace_root.display()
        ));
    }
    Ok(target_norm)
}

/// 专供 `read_file` 使用的路径解析：除工作区根目录内文件外，还只读放行全局 `~/.gcode/skills/` 目录以便按需加载全局技能。
pub fn resolve_readable_path(workspace_root: &Path, raw_path: &str) -> Result<PathBuf, String> {
    if let Ok(p) = resolve_workspace_path(workspace_root, raw_path) {
        return Ok(p);
    }
    let candidate = Path::new(raw_path);
    if candidate.is_absolute()
        && let Some(home) = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    {
        let global_skills = normalize_lexical(&home.join(".gcode").join("skills"));
        let target_norm = normalize_lexical(candidate);
        if target_norm.starts_with(&global_skills) {
            return Ok(target_norm);
        }
    }
    Err(format!(
        "路径越界拒绝访问：{raw_path} 不在工作区 {} 内",
        workspace_root.display()
    ))
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 构造键序固定的 JSON Schema 参数对象，保证 Prefix Cache 字节级稳定。
pub fn build_object_schema(
    properties: Vec<(&str, Value)>,
    required: Vec<&str>,
) -> BTreeMap<String, Value> {
    let mut props_map = serde_json::Map::new();
    let mut sorted = properties;
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in sorted {
        props_map.insert(k.to_string(), v);
    }

    let mut schema = BTreeMap::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), Value::Object(props_map));
    if !required.is_empty() {
        schema.insert("required".to_string(), json!(required));
    }
    schema
}
