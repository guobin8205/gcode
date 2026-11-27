use gcode_protocol::{Message, Role};

/// 用于协议与行为差异化适配的模型家族枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    DeepSeek,
    Glm,
    Kimi,
    OpenAi,
    Anthropic,
    Generic,
}

/// 封装模型特定怪癖（例如 DeepSeek/GLM/Kimi 在当前 Turn 工具调用链之间回传 `reasoning_content` 的规则）。
#[derive(Debug, Clone)]
pub struct ModelProfile {
    pub family: ModelFamily,
    pub model_id: String,
    /// 是否在当前 Turn 的多步工具调用之间回传 `reasoning_content`（跨新 User Turn 时自动剥离旧思考链）。
    pub require_reasoning_roundtrip_with_tools: bool,
    pub lock_sampling_params: bool,
    pub system_nudge: &'static str,
}

impl ModelProfile {
    pub fn for_model(family: ModelFamily, model_id: impl Into<String>) -> Self {
        match family {
            ModelFamily::DeepSeek => Self {
                family,
                model_id: model_id.into(),
                require_reasoning_roundtrip_with_tools: true,
                lock_sampling_params: true,
                system_nudge: "Act directly with tools; keep reasoning concise for simple reads.",
            },
            ModelFamily::Glm => Self {
                family,
                model_id: model_id.into(),
                require_reasoning_roundtrip_with_tools: true,
                lock_sampling_params: false,
                system_nudge: "Execute tools first and verify results; avoid lengthy preambles.",
            },
            ModelFamily::Kimi => Self {
                family,
                model_id: model_id.into(),
                require_reasoning_roundtrip_with_tools: true,
                lock_sampling_params: false,
                system_nudge: "Locate targets with grep/glob and read targeted line ranges first.",
            },
            _ => Self {
                family,
                model_id: model_id.into(),
                require_reasoning_roundtrip_with_tools: false,
                lock_sampling_params: false,
                system_nudge: "",
            },
        }
    }

    /// 按目标模型规范清洗消息历史（Interleaved Thinking / Turn 级思考回传）：
    /// - 仅在携带 `tools` 且开启思考回传时，保留**最后一个 User 消息之后**（当前 Turn 内多步工具调用之间）的 `reasoning_content`；
    /// - 更早历史 Turn 的 `reasoning_content` 一律剥离，既避免 DeepSeek/GLM/Kimi 工具链思维断链，又防止早期长思考撑爆上下文。
    pub fn prepare_messages(&self, messages: &[Message], has_tools: bool) -> Vec<Message> {
        let last_user_idx = messages.iter().rposition(|m| m.role == Role::User);
        let keep_current_turn_reasoning = has_tools && self.require_reasoning_roundtrip_with_tools;

        messages
            .iter()
            .enumerate()
            .map(|(idx, orig)| {
                let mut msg = orig.clone();
                let in_current_turn = last_user_idx.is_some_and(|u| idx > u);
                if !keep_current_turn_reasoning || !in_current_turn {
                    msg.reasoning_content = None;
                }
                msg
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_msg(role: Role, content: &str, reasoning: Option<&str>) -> Message {
        Message {
            role,
            content: content.to_string(),
            reasoning_content: reasoning.map(str::to_string),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    #[test]
    fn keeps_reasoning_only_in_current_turn_when_tools_enabled() {
        let history = vec![
            mk_msg(Role::User, "Turn 1", None),
            mk_msg(Role::Assistant, "Ans 1", Some("old thinking")),
            mk_msg(Role::User, "Turn 2", None),
            mk_msg(Role::Assistant, "", Some("current turn thinking")),
            mk_msg(Role::Tool, "tool output", None),
        ];

        for family in [ModelFamily::DeepSeek, ModelFamily::Glm, ModelFamily::Kimi] {
            let profile = ModelProfile::for_model(family, "test-model");

            let with_tools = profile.prepare_messages(&history, true);
            assert_eq!(with_tools[1].reasoning_content, None);
            assert_eq!(
                with_tools[3].reasoning_content.as_deref(),
                Some("current turn thinking")
            );

            let without_tools = profile.prepare_messages(&history, false);
            assert_eq!(without_tools[1].reasoning_content, None);
            assert_eq!(without_tools[3].reasoning_content, None);
        }
    }
}
