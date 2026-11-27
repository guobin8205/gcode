use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use thiserror::Error;

use crate::adapter::ModelFamily;
use crate::anthropic::AnthropicProvider;
use crate::openai::OpenAiCompatibleProvider;
use crate::provider::ModelProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiProtocol {
    OpenAiCompatible,
    Anthropic,
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model_id: String,
    pub family: ModelFamily,
    pub protocol: ApiProtocol,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("无法读取配置文件 {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("配置文件 JSON 解析失败: {0}")]
    Json(#[from] serde_json::Error),
    #[error("配置缺少必要字段: {0}")]
    MissingField(&'static str),
    #[error("未知 profile: {0}")]
    UnknownProfile(String),
}

#[derive(Debug, Deserialize)]
struct ClaudeSettingsFile {
    #[serde(default)]
    env: HashMap<String, String>,
}

/// 将 Anthropic 风格的接口地址映射为对应的 OpenAI 兼容地址。
/// 如果不认识该地址，则原样返回（通常已是 OpenAI 兼容端点）。
pub fn map_anthropic_url_to_openai(base_url: &str) -> String {
    let u = base_url.to_lowercase();
    if u.contains("api.deepseek.com") {
        // https://api.deepseek.com/anthropic → https://api.deepseek.com/v1
        "https://api.deepseek.com/v1".to_string()
    } else if u.contains("api.moonshot.cn") {
        // https://api.moonshot.cn/anthropic → https://api.moonshot.cn/v1
        "https://api.moonshot.cn/v1".to_string()
    } else if u.contains("open.bigmodel.cn") {
        // 智谱 Coding 套餐 OpenAI 兼容端点：https://open.bigmodel.cn/api/coding/paas/v4
        "https://open.bigmodel.cn/api/coding/paas/v4".to_string()
    } else if u.contains("aicoding.dobest.com") {
        // 公司内网转发端点：OpenAI 协议需走 /v1
        let trimmed = base_url.trim_end_matches('/');
        if trimmed.ends_with("/v1/anthropic") {
            trimmed.trim_end_matches("/anthropic").to_string()
        } else if trimmed.ends_with("/v1") {
            trimmed.to_string()
        } else {
            format!("{trimmed}/v1")
        }
    } else {
        base_url.to_string()
    }
}

/// 根据模型名与接口地址自动推断模型家族。
pub fn infer_model_family(model_id: &str, base_url: &str) -> ModelFamily {
    let m = model_id.to_lowercase();
    let u = base_url.to_lowercase();
    if m.contains("deepseek") || u.contains("deepseek") {
        ModelFamily::DeepSeek
    } else if m.contains("glm") || u.contains("bigmodel") || u.contains("zhipu") || u.contains("dobest") {
        ModelFamily::Glm
    } else if m.contains("kimi") || u.contains("moonshot") {
        ModelFamily::Kimi
    } else if m.contains("claude") {
        ModelFamily::Anthropic
    } else if m.contains("gpt") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") {
        ModelFamily::OpenAi
    } else {
        ModelFamily::Generic
    }
}

/// 内置 profile 配置——无需 `.claude/settings_*.json` 文件即可使用，API Key 从环境变量读取。
pub fn builtin_profile(profile: &str) -> Option<ProviderConfig> {
    let env_key = |names: &[&str]| -> String {
        for name in names {
            if let Ok(v) = std::env::var(name)
                && !v.trim().is_empty()
            {
                return v;
            }
        }
        String::new()
    };

    match profile {
        "deepseek" => Some(ProviderConfig {
            base_url: "https://api.deepseek.com/v1".to_string(),
            api_key: env_key(&["DEEPSEEK_API_KEY", "GCODE_API_KEY"]),
            model_id: "deepseek-chat".to_string(),
            family: ModelFamily::DeepSeek,
            protocol: ApiProtocol::OpenAiCompatible,
        }),
        "kimi" => Some(ProviderConfig {
            base_url: "https://api.moonshot.cn/v1".to_string(),
            api_key: env_key(&["MOONSHOT_API_KEY", "KIMI_API_KEY", "GCODE_API_KEY"]),
            model_id: "kimi-k2.5".to_string(),
            family: ModelFamily::Kimi,
            protocol: ApiProtocol::OpenAiCompatible,
        }),
        // zhipu / glm 指向智谱 Coding 套餐 OpenAI 兼容端点
        "zhipu" | "glm" => Some(ProviderConfig {
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4".to_string(),
            api_key: env_key(&["ZHIPU_API_KEY", "GLM_API_KEY", "GCODE_API_KEY"]),
            model_id: "glm-5".to_string(),
            family: ModelFamily::Glm,
            protocol: ApiProtocol::OpenAiCompatible,
        }),
        _ => None,
    }
}

/// 从指定路径的 Claude Code `settings_*.json` 加载模型配置。
/// 加载后自动将 Anthropic URL 映射为 OpenAI 兼容 URL，协议默认为 OpenAI 兼容。
pub fn load_from_claude_settings_path(path: impl AsRef<Path>) -> Result<ProviderConfig, ConfigError> {
    let path_ref = path.as_ref();
    let raw = std::fs::read_to_string(path_ref).map_err(|source| ConfigError::Io {
        path: path_ref.to_path_buf(),
        source,
    })?;
    let parsed: ClaudeSettingsFile = serde_json::from_str(&raw)?;

    let raw_base_url = parsed
        .env
        .get("ANTHROPIC_BASE_URL")
        .cloned()
        .ok_or(ConfigError::MissingField("env.ANTHROPIC_BASE_URL"))?;
    let api_key = parsed
        .env
        .get("ANTHROPIC_AUTH_TOKEN")
        .or_else(|| parsed.env.get("ANTHROPIC_API_KEY"))
        .cloned()
        .ok_or(ConfigError::MissingField("env.ANTHROPIC_AUTH_TOKEN"))?;
    let model_id = parsed
        .env
        .get("ANTHROPIC_MODEL")
        .or_else(|| parsed.env.get("ANTHROPIC_DEFAULT_SONNET_MODEL"))
        .cloned()
        .ok_or(ConfigError::MissingField("env.ANTHROPIC_MODEL"))?;

    // 将 Anthropic 风格 URL 自动转换为 OpenAI 兼容 URL
    let base_url = map_anthropic_url_to_openai(&raw_base_url);
    let family = infer_model_family(&model_id, &base_url);

    Ok(ProviderConfig {
        base_url,
        api_key,
        model_id,
        family,
        // 统一使用 OpenAI 兼容协议——国产模型均已支持
        protocol: ApiProtocol::OpenAiCompatible,
    })
}

/// 按 profile 名称加载配置。
/// 查找顺序：
///   1. `~/.claude/settings_<profile>.json`（profile == "default" 时为 `settings.json`）
///   2. 内置 profile（deepseek / kimi / zhipu / glm）
///   3. 返回 `ConfigError::UnknownProfile`
pub fn load_claude_profile(profile: &str) -> Result<ProviderConfig, ConfigError> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);

    // 先尝试从磁盘文件加载
    if let Some(home) = &home {
        let file_name = if profile == "default" {
            "settings.json".to_string()
        } else {
            format!("settings_{profile}.json")
        };
        let settings_path = home.join(".claude").join(file_name);
        if settings_path.exists() {
            return load_from_claude_settings_path(settings_path);
        }
    }

    // 降级到内置 profile
    if let Some(cfg) = builtin_profile(profile) {
        return Ok(cfg);
    }

    Err(ConfigError::UnknownProfile(profile.to_string()))
}

/// 根据协议类型构建对应的 `ModelProvider` 实例。
pub fn build_provider(config: &ProviderConfig) -> Arc<dyn ModelProvider> {
    match config.protocol {
        ApiProtocol::OpenAiCompatible => Arc::new(OpenAiCompatibleProvider::new(config.clone())),
        ApiProtocol::Anthropic => Arc::new(AnthropicProvider::new(config.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_settings_json_and_infers_family() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("gcode_test_settings_{}.json", std::process::id()));
        let json = r#"{
          "env": {
            "ANTHROPIC_AUTH_TOKEN": "sk-test-token",
            "ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic",
            "ANTHROPIC_MODEL": "deepseek-flash"
          }
        }"#;
        std::fs::write(&file, json).unwrap();

        let cfg = load_from_claude_settings_path(&file).unwrap();
        let _ = std::fs::remove_file(&file);

        assert_eq!(cfg.family, ModelFamily::DeepSeek);
        // URL 应已被映射为 OpenAI 兼容端点
        assert_eq!(cfg.base_url, "https://api.deepseek.com/v1");
        // 协议应为 OpenAI 兼容
        assert_eq!(cfg.protocol, ApiProtocol::OpenAiCompatible);
        assert_eq!(cfg.model_id, "deepseek-flash");
    }

    #[test]
    fn map_anthropic_url_to_openai_deepseek() {
        assert_eq!(
            map_anthropic_url_to_openai("https://api.deepseek.com/anthropic"),
            "https://api.deepseek.com/v1"
        );
    }

    #[test]
    fn map_anthropic_url_to_openai_kimi() {
        assert_eq!(
            map_anthropic_url_to_openai("https://api.moonshot.cn/anthropic"),
            "https://api.moonshot.cn/v1"
        );
    }

    #[test]
    fn map_anthropic_url_to_openai_bigmodel() {
        assert_eq!(
            map_anthropic_url_to_openai("https://open.bigmodel.cn/api/anthropic"),
            "https://open.bigmodel.cn/api/coding/paas/v4"
        );
    }

    #[test]
    fn map_anthropic_url_passthrough_dobest() {
        // 内网端点自动规范为 /v1
        assert_eq!(
            map_anthropic_url_to_openai("https://aicoding.dobest.com"),
            "https://aicoding.dobest.com/v1"
        );
    }

    #[test]
    fn builtin_profile_zhipu() {
        let cfg = builtin_profile("zhipu").unwrap();
        assert_eq!(cfg.family, ModelFamily::Glm);
        assert_eq!(cfg.protocol, ApiProtocol::OpenAiCompatible);
        assert!(cfg.base_url.contains("open.bigmodel.cn/api/coding/paas/v4"));
    }

    #[test]
    fn builtin_profile_deepseek() {
        let cfg = builtin_profile("deepseek").unwrap();
        assert_eq!(cfg.family, ModelFamily::DeepSeek);
        assert_eq!(cfg.protocol, ApiProtocol::OpenAiCompatible);
    }

    #[test]
    fn builtin_profile_kimi() {
        let cfg = builtin_profile("kimi").unwrap();
        assert_eq!(cfg.family, ModelFamily::Kimi);
        assert_eq!(cfg.protocol, ApiProtocol::OpenAiCompatible);
        assert_eq!(cfg.model_id, "kimi-k2.5");
    }

    #[test]
    fn builtin_profile_unknown() {
        assert!(builtin_profile("unknown_xyz").is_none());
    }

    #[test]
    fn infer_model_family_dobest_is_glm() {
        // aicoding.dobest.com 应识别为 GLM 家族
        let f = infer_model_family("glm-5.2-local", "https://aicoding.dobest.com");
        assert_eq!(f, ModelFamily::Glm);
    }
}
