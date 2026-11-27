use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use clap::{Parser, ValueEnum};
use gcode_core::{
    AgentRuntime, ApprovalHandler, AutoApproveHandler, ChannelApprovalHandler, EventStore,
    build_default_system_prompt,
};
use gcode_models::{
    ApiProtocol, ModelProfile, ProviderConfig, build_provider, infer_model_family,
    load_claude_profile,
};
use gcode_protocol::{EventKind, ServerNotification};
use gcode_tools::default_core_registry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProtocolArg {
    Openai,
    Anthropic,
}

#[derive(Debug, Parser)]
#[command(name = "gcode", about = "终端优先、模型无关的编码 Agent")]
struct Cli {
    /// 读取 ~/.claude/settings_<profile>.json 中的模型与接口配置（如 deepseek、zhipu、kimi、glm）
    #[arg(short, long)]
    profile: Option<String>,

    /// 模型标识（例如 deepseek-reasoner、glm-5、kimi-k2）
    #[arg(short, long, default_value = "deepseek-reasoner")]
    model: String,

    /// 自定义 API Base URL（未指定 --profile 时生效）
    #[arg(long)]
    base_url: Option<String>,

    /// API Key（也可通过环境变量 GCODE_API_KEY / DEEPSEEK_API_KEY 提供）
    #[arg(long)]
    api_key: Option<String>,

    /// 接口协议类型（标准 openai 或 anthropic 兼容）
    #[arg(long, value_enum, default_value = "openai")]
    protocol: ProtocolArg,

    /// 工作区根目录
    #[arg(short, long, default_value = ".")]
    workspace: PathBuf,

    /// 自动放行所有工具调用（跳过交互式确认弹窗）
    #[arg(long)]
    yolo: bool,

    /// 自动恢复当前工作区最近一次会话
    #[arg(short = 'c', long = "continue")]
    continue_latest: bool,

    /// 按会话 ID 恢复历史会话（对应 .gcode/sessions/<id>.jsonl）
    #[arg(short = 'r', long)]
    resume: Option<String>,

    /// 无头单次执行模式：传入单条指令后直接输出结果并退出
    #[arg(long)]
    prompt: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let workspace = clean_canonical_path(&cli.workspace);

    let config = if let Some(profile) = &cli.profile {
        load_claude_profile(profile)?
    } else {
        let base_url = cli
            .base_url
            .or_else(|| std::env::var("GCODE_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.deepseek.com/v1".to_string());
        let api_key = cli
            .api_key
            .or_else(|| std::env::var("GCODE_API_KEY").ok())
            .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok())
            .unwrap_or_default();
        let protocol = match cli.protocol {
            ProtocolArg::Openai => ApiProtocol::OpenAiCompatible,
            ProtocolArg::Anthropic => ApiProtocol::Anthropic,
        };
        let family = infer_model_family(&cli.model, &base_url);
        ProviderConfig {
            base_url,
            api_key,
            model_id: cli.model.clone(),
            family,
            protocol,
        }
    };

    let sessions_dir = workspace.join(".gcode").join("sessions");
    let (store, resumed) = if let Some(target_id) = &cli.resume {
        let path = sessions_dir.join(format!("{target_id}.jsonl"));
        if !path.exists() {
            anyhow::bail!("未找到会话文件: {}", path.display());
        }
        (EventStore::open_or_create(&path)?, true)
    } else if cli.continue_latest {
        if let Some((_id, path)) = EventStore::find_latest_session_path(&workspace)? {
            (EventStore::open_or_create(&path)?, true)
        } else {
            let session_id = Utc::now().format("%Y%m%d_%H%M%S").to_string();
            let path = sessions_dir.join(format!("{session_id}.jsonl"));
            (EventStore::open_or_create(&path)?, false)
        }
    } else {
        let session_id = Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        (EventStore::open_or_create(&path)?, false)
    };

    let mut store = store;
    if !resumed {
        let session_id = Utc::now().format("%Y%m%d_%H%M%S").to_string();
        store.append(
            0,
            None,
            EventKind::SessionStarted {
                session_id,
                workspace: workspace.display().to_string(),
                model_id: config.model_id.clone(),
            },
        )?;
    }
    let next_turn_id = store.last_turn_id() + 1;

    let provider = build_provider(&config);
    let profile = ModelProfile::for_model(config.family, &config.model_id);
    let system_prompt = build_default_system_prompt(&workspace);
    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel();

    let (approval, broker): (Arc<dyn ApprovalHandler>, Option<Arc<ChannelApprovalHandler>>) =
        if cli.yolo || cli.prompt.is_some() {
            (Arc::new(AutoApproveHandler), None)
        } else {
            let b = ChannelApprovalHandler::new(notif_tx.clone());
            (b.clone(), Some(b))
        };

    let mut runtime = AgentRuntime::new(
        &workspace,
        provider,
        profile,
        default_core_registry(),
        store,
        approval,
        system_prompt,
    );
    runtime.enable_subagent_tool();

    if let Some(prompt) = cli.prompt {
        let printer = tokio::spawn(async move {
            while let Some(notif) = notif_rx.recv().await {
                match notif {
                    ServerNotification::StreamDelta {
                        text_delta: Some(t),
                        ..
                    } => print!("{t}"),
                    ServerNotification::EventAppended { event } => {
                        if let EventKind::ToolResult {
                            tool_name,
                            is_error,
                            ..
                        } = event.kind
                        {
                            let mark = if is_error { "✗" } else { "✓" };
                            eprintln!("\n[{mark} {tool_name}]");
                        }
                    }
                    ServerNotification::StatusMessage { message } => {
                        eprintln!("\n[状态] {message}");
                    }
                    _ => {}
                }
            }
        });
        runtime
            .run_turn(next_turn_id, prompt, CancellationToken::new(), &notif_tx)
            .await?;
        drop(notif_tx);
        let _ = printer.await;
        println!();
        return Ok(());
    }

    gcode_tui::run_tui_with_runtime(runtime, broker, notif_tx, notif_rx).await
}

/// 规范化路径并去除 Windows `std::fs::canonicalize` 附加的 `\\?\` 前缀，避免影响显示与 PowerShell 执行。
fn clean_canonical_path(path: &std::path::Path) -> PathBuf {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(windows)]
    {
        let s = resolved.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{stripped}"));
        }
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    resolved
}
