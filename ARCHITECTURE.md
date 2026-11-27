# `gcode` 架构设计规范

> **设计目标：** 基于 **Rust + Ratatui** 构建的终端优先（Terminal-First）、模型无关编码 Agent，具备比肩 Claude Code / Codex 的工程质感与交互体验，同时针对 **DeepSeek、GLM、Kimi** 等国产模型实现一等公民级别的协议适配、高编辑成功率与极致的 Prefix Cache 成本优化。

---

## 1. 设计谱系与核心理念

`gcode` 融合了四大标杆 Agent 系统的核心工程优势：

| 参考对象 | 吸收的核心模式 | 在 `gcode` 中的落地方式 |
| :--- | :--- | :--- |
| **DeepSeek Harness (`dsh`)** | *“Model-visible means logged”*、事件溯源 + 投影、Turn/Step 双层粒度 | 以 Append-Only JSONL 事件流作为唯一事实源；UI、上下文、指标均由事件流折叠投影而出，原生支持 `fork` 与 `replay`。 |
| **Claude Code** | 终端优先 Rich TUI、严格 Prefix Cache 排序、3 级权限治理、生命周期 Hooks、Abort Tree | `PreToolUse`/`PostToolUse` 外部脚本钩子、`Allow`/`Ask`/`Deny` 权限策略，以及基于 `CancellationToken` 的级联取消。 |
| **OpenClaw** | 两级工具分层（`SKILL.md`）、*“无隐藏状态”* Markdown 记忆、Silent Memory Flush | 常驻核心工具精简至 ~7 个，领域能力按需读取 `SKILL.md`；压缩上下文前执行静默轮次将关键记忆落盘至 `.gcode/MEMORY.md`。 |
| **OpenAI Codex (`codex-rs`)** | Headless 内核 + Rust 单二进制分发、内置 `ripgrep`、Git Worktree 子代理隔离 | 零依赖快速启动的 CLI/TUI 二进制、原生 `ignore`/`grep-searcher` 搜索，以及基于 Worktree 隔离的并行子代理。 |

---

## 2. Workspace 模块拓扑

```text
gcode/
├── Cargo.toml                  # Workspace 编排
├── ARCHITECTURE.md             # 本架构规范文档
└── crates/
    ├── gcode-protocol/         # 1. 共享协议、事件日志 Schema、消息/工具契约、RPC 定义
    ├── gcode-models/           # 2. ModelProvider Trait、SSE 容错解析、DeepSeek/GLM/Kimi 适配器
    ├── gcode-tools/            # 3. 内置工具、Hashline 编辑器、ripgrep 搜索、SKILL.md 加载器
    ├── gcode-core/             # 4. Headless 运行时：Master Loop、Event Store、压缩与权限治理
    ├── gcode-tui/              # 5. Ratatui 富终端界面（流式 Markdown、Diff 预览、思考链折叠）
    └── gcode-cli/              # 6. `gcode` 二进制入口（参数解析、配置加载、启动 TUI/Headless）
```

**依赖隔离原则：** `gcode-protocol` 不引入任何重型运行时依赖；`gcode-core` 绝不依赖 `gcode-tui`，确保内核 100% 无头化（Headless），未来新增 `gcode-server`（Web/桌面看板）时可直接挂载同一套事件流。

---

## 3. 核心架构不变量

### 3.1 Model-Visible Means Logged（事件溯源 + 状态投影）
* 所有会影响模型上下文或工作区状态的变化，必须先以单调递增的 `seq`、时间戳、`turn_id`、`step_id` 及强类型 `EventKind` 追加写入 `.gcode/sessions/<session_id>.jsonl`。
* **Turn 与 Step 分离：**
  * **Turn：** 用户发起的一次任务或目标边界。
  * **Step：** Turn 内部的一次 `LLM 推理 -> 工具执行` 循环迭代。
* **状态投影（Projection）：** 对话上下文、Token/缓存统计、Todo 列表及 TUI 状态均通过对事件流执行纯函数折叠（Fold）计算得出。
* **Git 轻量快照联动：** 在 Turn 中首次修改文件前，`gcode-core` 通过轻量 Git Tree 哈希（如 `git write-tree`，不污染用户提交历史）记录磁盘快照并写入事件流。执行 `Turn Fork` 或 `Rollback` 时，同步回滚对话记忆与磁盘文件。

### 3.2 字节级稳定的 Prefix Caching 契约
DeepSeek、GLM、Kimi 在请求前缀稳定时可获得 95%+ 的缓存命中与显著降本。每次 LLM 请求必须严格按以下易变性顺序拼装：
1. **静态 System Base**（角色设定、核心纪律、Hashline 使用规范）
2. **工具声明列表**（JSON Schema 使用 `BTreeMap` 固定键序，**会话期间绝不动态增删或重排**）
3. **项目规则**（`GCODE.md` / `AGENTS.md`）
4. **Append-Only 对话历史**（历史轮次、工具调用与返回）

> **铁律：** 任何运行时临时提示（如当前时间、权限拒绝说明、预算警告）只能追加在最新一条 User/Tool 消息尾部，严禁修改前方静态前缀。

### 3.3 模型适配层（DeepSeek / GLM / Kimi）
通用 OpenAI 包装层在国产模型上容易失效，`gcode-models` 通过 `ModelProvider` + `ModelProfile` 收拢所有差异：
* **DeepSeek：**
  * 在 `AssistantMessage` 事件中完整保留 `reasoning_content`。
  * 当请求携带 `tools` 时，严格将历史 `reasoning_content` 回传给 API；无 `tools` 时按规范剥离。
  * 在思考模式下自动锁定或过滤受限采样参数（如 `temperature`、`top_p`）。
* **GLM：**
  * 固定工具 Schema 键序以吃满前缀缓存。
  * 通过 Profile 约束强化 *Tool-First* 行为，抑制行动前的冗长解释。
* **Kimi：**
  * 强化长上下文纪律：先通过 `grep`/`glob` 定位行号，再按行范围分段读取大文件。
* **流式容错：** SSE 解析器需兼容工具参数 JSON 分片、缺失或乱序的 `index` 字段及空 delta 帧。

### 3.4 Hashline 优先的代码编辑协议
为彻底解决非 OpenAI 模型在 `str_replace` 下的空格/缩进幻觉以及 `apply_patch` 下的统一 Diff 语法漂移问题：
* **`read_file`** 默认输出带 `行号:4位xxh32十六进制哈希| 内容` 前缀的文本：
  ```text
  42:a3f1| fn main() {
  43:0b9c|     println!("hello");
  44:7e12| }
  ```
* **`edit_file`** 支持：
  1. **Hashline 锚点模式（主推）：** 通过 `start_anchor`（如 `"42:a3f1"`）与可选 `end_anchor` 指定替换、插入或删除范围，无需复述原代码。
  2. **过期读取保护（Stale-Read Protection）：** 若文件在两次读取间被外部改动，哈希校验立即失败并返回最新上下文，避免静默写坏文件。
  3. **模糊 `str_replace` 兜底：** 必要时提供带缩进归一化的文本替换兜底。

### 3.5 两级工具分层（约 7 个核心工具 + `SKILL.md`）
精简常驻工具集可显著降低模型参数幻觉并节省前缀 Token：
1. `read_file`（支持行范围 + Hashline 输出）
2. `edit_file`（Hashline 主编辑 + `str_replace` 兜底）
3. `write_file`（新建/覆盖文件）
4. `shell`（跨平台命令执行，支持超时与进程树清理）
5. `grep` / `glob`（基于原生 `ignore`/`grep-searcher`，无需外部二进制）
6. `spawn_agent`（隔离上下文的子代理，支持 Git Worktree 隔离）
7. `todo`（基于事件流投影的任务追踪）

领域工作流通过 `.gcode/skills/*/SKILL.md` 挂载：常驻上下文仅暴露名称与一句话触发摘要，模型按需调用 `read_file` 加载完整指令。

### 3.6 两段式上下文压缩
1. **Micro-Compaction（无损优先）：** 优先对历史轮次中过大的 `shell` / `read_file` 输出做截断或引用替换，保留近期推理与编辑轨迹。
2. **Silent Memory Flush（静默记忆落盘）：** 在总结或裁剪旧轮次前，先执行一次受限静默轮次，引导模型将关键架构决策、项目约定和踩坑记录写入 `.gcode/MEMORY.md`。

### 3.7 权限治理、Hooks 与子代理隔离
* **权限策略：** 每次工具执行前经过 `Allow` / `Ask` / `Deny` 规则判定。
* **生命周期 Hooks：** `PreToolUse` 与 `PostToolUse` 通过 `stdin`/`stdout` JSON 调用外部脚本，支持确定性拦截或自动格式化。
* **Abort Tree：** 基于 `tokio_util::sync::CancellationToken` 串联父 Turn 与所有子代理、Shell 子进程，按下 `Esc` 时级联清理。
