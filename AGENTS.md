# AGENTS.md

本文件为 各类智能体 在本仓库工作时提供指引。**指令优先级高于默认行为，必须严格遵循。**

> 📖 **产品设计与文档导航**：请先阅读 [docs/production-design/00-overview.md](docs/production-design/00-overview.md) 了解产品定位、里程碑与完整文档体系。改核心前必读对应 phases 文档。

更新时间 20260727

---

## 1. 项目概览

Blink 是一个 Windows 全局快捷入口，定位不只是「启动器」。**目标：做一个极其丝滑的启动器，并且把常用的功能都丝滑融合，使用 Chord 模式来调用各种增强能力，不止是启动器。**
终极目标：感知用户上下文、主动推荐动作，让任何操作都比原来的路径更快。

当前 **0.12.0-0.12.6 全部完成**（基础设施 → 对话窗口 → 体验优化 → 体验修复 → 功能增强 → 对话分组：多层文件夹 + 分组系统提示词 + 拖拽排序 + 折叠持久化 + 内联管理），测试基线 **875 通过**。详见 [phases/0.12-ai-ecosystem.md](docs/production-design/phases/0.12-ai-ecosystem.md)。

- ✅ **0.10 语音输入**：STT + 语音打字（G1 主窗口语音输入 / G2 语音输入法上屏）。工具箱层定 FunASR（SenseVoice 准确率 7.81%，CPU 17× 实时）。详见 [phases/0.10-voice-agent.md](docs/production-design/phases/0.10-voice-agent.md)
  - 0.10.4：伪流式 VAD 切句 + 累积预览 + 移除真流式 + 架构清理
  - 0.10.3：blink_stt_server 统一服务 + SendInput Unicode + 热词/ITN
  - 0.10.5：收尾体验优化（VAD 参数滑动条 + 热词/高级选项 UI 优化）
  - 0.10.5 TSF Composition / 0.10.6 hook 吞键：均已废弃（跨进程不可用 / Alt keyup 副作用不可控），回归 SendInput + Clipboard 两级
  - 0.10.7：Chord 交互统一化——语音 chord 化 + hold 门禁生效 + chord 键位可配置 + 主窗 Alt 独占（hook 吞 chord keydown）+ 设置页展开式改造（accordion + 键位录制 + 剪贴板详细配置迁回）
  - 0.10.8：呈现共存策略收敛（BuiltinEngine 空 query Context-only item 标 context_aware + chordEligible 跳过，修「剪贴板 URL 时 Alt 不触发 chord」）+ statusbar 双行 stack（Ghost 主行 + Chord 副行，修 flex space-between 撕裂）+ 全站 emoji 换 Lucide 图标包（29 处，本地 SVG sprite）。详见 [phases/0.10-voice-agent.md §十一](docs/production-design/phases/0.10-voice-agent.md#十一0108--呈现共存策略收敛--图标包接入规划中)
- ✅ **0.11 插件通信契约重设计 + AI 调用插件链路完善 + 截图标注增强**：主线修 0.9.3 遗留（tool-call 结果截断/语义混淆/无回流/元信息不友好）+ 补 AI tool 池缺口（应用搜索 / 剪贴板历史 Capability）+ builtin 插件全 Rust 化（翻译插件，一次性 1:1 迁移）+ 截图升级到 PixPin 级现代交互 + 截图能力化重构。详见 [phases/0.11-plugin-ai-toolchain.md](docs/production-design/phases/0.11-plugin-ai-toolchain.md)
  - 0.11.0：结果模型统一（PluginItem payload + ActionOutcome::Items + 统一投影）+ AI 结果视觉形态
  - 0.11.1：工具元信息增强（manifest 新字段 + 参数动态注入 + sensitive 铺路）
  - 0.11.2：应用搜索 Capability（search_apps，共享 StartMenuEngine）
  - 0.11.3：提示词统一管理（ai/prompt.rs + 工具列表含参数 + token 监控）
  - 0.11.4：结果回流 AI（Turn 2 tool chain + 三态配置 + 审计日志 + 占位文案/过渡态/错误展示/自动执行反馈）
  - 0.11.5：剪贴板历史 Capability + 搜索型抽象评估（决定不抽）
  - 0.11.6：翻译插件全量 Rust 化（5 引擎 1:1 迁移）
  - 0.11.7：截图标注增强（选区停留 + 7 种标注 + OCR + 钉图 + 保存 PNG/JPEG）+ 能力化重构（ScreenshotBackend/OcrBackend trait + list_displays + Fake 后端）
  - 0.11.8：截图优化收尾 + 图标包引入（Lucide sprite）
  - 0.11.9：OCR word 级链路（`OcrLine.Words()` + word bounding_rect + 智能拼接替代前端强清）+ 原图阅读模式（word 拖选 + textarea 双向联动）+ 翻译衔接（`translate_text` command 直调 translate 插件 tool；OCR 面板双 tab 原文/译文；工具栏「OCR」「翻译」共用面板）+ 水印独立图层（`watermarkConfig` 单例覆盖式，脱离 commands 撤销栈）+ 钉图窗口右键 OCR/翻译
- ✅ **0.11.10 图上翻译 + 截图交互重构**：`overlayLayer` 图层引擎（原位嵌原文/译文，背景遮罩三档+字号自适应）+ 选取工具（默认激活，解决鼠标层冲突）+ 预热 OCR（按钮秒响应）+ 双按钮单一路径（[识别]/[翻译]各自清晰，消除中间态）+ 面板抽屉化（默认展开，可拖动）+ 误点保护（点选区外 no-op）+ `translate_batch` 批量翻译 + 命名迁移 OCR→识别。测试基线 **783 通过**。详见 [phases/0.11-plugin-ai-toolchain.md §2.10](docs/production-design/phases/0.11-plugin-ai-toolchain.md#210-图上翻译--截图交互重构01110)
- ✅ **0.12 AI 能力架构搭建**（0.12.0-0.12.4 全部完成）：基础设施抽取与清账（投影统一 ✅ / **DB 四层拆分** ✅ / **Provider 模型统一管理** ✅ / ollama Provider 接入 ✅ / **Tool 适配层** ✅ / CapabilityRegistry 动态注册 ✅）+ 对话窗口★（独立 Agent 窗口 + **Alt+Q chord** + rig AgentBuilder）+ 对话机制（conversation 隔离 + **SQLite ConversationMemory** + 滑动窗口 20 条 + tool loop 50 turns 触顶）+ Chat 体验（思考块 / 无边框 / 模型选择 / 复制按钮 / Tool 结果折叠 / Token 用量 / 语音输入）+ 多对话管理（侧边栏 / 列表切换 / 删除 / 重命名 / 历史恢复）+ **0.12.4 体验修复**（15 项 bug 修复与优化：侧边栏切换 / 模型下拉 / 语音输入根因 / 对话标题 / 模型选择器移至输入框 / **工具调用渲染修复** / 消息宽度优化）。**产品边界：Blink 不做 AI 运行时，靠外部 ollama/lmstudio**。详见 [phases/0.12-ai-ecosystem.md](docs/production-design/phases/0.12-ai-ecosystem.md)
- ✅ **0.12.5 功能增强**（全部完成）：对话窗口引导泡泡（空状态展示示例 prompt，点击预填充）/ 对话标题 LLM 自动生成（设置开关 + 模型档位选择）/ 设置页 AI Tab 对话配置区（ChatConfig 子结构）/ 消息编辑重发（truncate_messages + isEdit 跳过标题）/ 导出对话为 Markdown（Tauri dialog.save + save_text_file）/ 代码块语法高亮（highlight.js + Catppuccin 主题）/ LLM 报错友好展示 + stack overflow 修复。测试基线 858 通过。详见 [phases/0.12-ai-ecosystem.md §五](docs/production-design/phases/0.12-ai-ecosystem.md)
- ✅ **0.12.6 对话分组**：参考元宝网页端——侧边栏多层分组层级 + 每组系统提示词 + 拖拽排序 + 折叠持久化 + 内联管理。详见 [phases/0.12-ai-ecosystem.md §五](docs/production-design/phases/0.12-ai-ecosystem.md)
- ✅ **0.12.7 对话窗口布局体系化**：面包屑标题✅（文件夹路径+对话标题，仅标题可编辑）+ 悬浮 z-index✅ + Signal 信号消息✅（IM 式居中提示：renderSignal + 停止/编辑/上限/错误场景）+ 时间分隔符✅（后端 created_at 透传 + >5min 插入 + 智能日期格式）+ 系统提示词横幅✅（新 IPC + 可滚动非 sticky + 折叠/关闭）+ 工具卡片增强✅（参数预览 + 耗时显示 + JSON 格式化 + header 布局）+ 消息间距优化✅（轮次间距分级：用户消息大间距 / 助手小间距 / Signal 中间距）。测试基线 **875 通过**。
- 🔜 **0.13 AI 调用能力扩展（基础版 + 开放，零嵌入模型依赖）**：MCP client（消费外部 tool，McpTool 进适配层 + tool 可见性控制 + 外部 tool 统一入口）/ **MCP server（0.13.4，护城河——正向投影 + 暴露 Blink 能力 + 授权 + 审计，与 client 对称）** / **自身 CLI 化（0.13.5，blink chat/search/screenshot/mcp-server，不启动 GUI）** / ✅ **token-aware context 压缩（0.13.1 已完成——条数窗口 → token 估算 + 接近上限压缩，自建策略不接 rig hook）** / ✅ **记忆 FTS5 召回（0.13.2 已完成——SQLite FTS5 全文检索，load() 裁剪时归档 + BM25 排序 + trigram 中文分词 + `<memory>` 标签注入，零嵌入模型依赖）** / ✅ **Skill 约定式（0.13.3 已完成——SKILL.md 三层目录发现 [Blink `%APPDATA%\blink\skills\` / Claude `~/.claude/skills/` / ZCode `~/.zcode/skills/`] + preamble 渐进式披露 [阶段1摘要常驻 + 阶段2触发全文注入] + `/skill` 显式激活 + `@source` 消歧 + 关键词/正则自动触发 + 设置页来源开关 + 刷新 + 列表展示 + 对话窗口 /skill 提示弹层）**。**核心原则：零嵌入模型依赖——所有功能在用户只有 chat 模型时也完整可用。Skill ≠ Tool：Skill 注入 preamble（教 AI 怎么做），Tool 进 tool 池（让 AI 能做什么）。**测试基线 **955 通过**。详见 [phases/0.13-ai-capability-expansion.md](docs/production-design/phases/0.13-ai-capability-expansion.md)
- 🔜 **0.14 AI 调用能力扩展（向量版）**：向量基础设施（zvec 阿里轻量向量库 + embedding 模型管理 + 统一存储/检索）/ 记忆向量召回（混合检索 FTS5 BM25 + zvec cosine，升级 0.13.2）/ RAG 知识库（文档处理流水线 + 混合检索 + search_knowledge_base Capability）/ AI 生成 Skill（LLM 从 --help 生成 SKILL.md）。详见 [phases/0.14-ai-vector-moat.md](docs/production-design/phases/0.14-ai-vector-moat.md)
- 🔮 **0.15+ 候选（未定案）**：**外部 agent 作 subagent**（把 opencode/pi/claude-code 当 subagent 调用，支持「整理下载文件夹」类文件长任务——rig 支持 agent-as-tool 模式，复用 0.12.0 ToolDyn 适配层零新概念，详见 [灵感卷](docs/production-design/inspiration-external-agent-subagent.md)）/ 事实记忆（tool-based，ChatGPT 式 memory）/ proactivity 主动建议深化。
- 🧭 **架构决策**：**Agent 后端坚持 rig-core 自建，不用 opencode/pi 当执行端**——opencode/pi 是和 Blink 同层的 agent 产品（非依赖），外包执行端会违反「不做 AI 运行时」边界、报废 0.12-0.14 架构投入。现成 agent 的正确用法是当 subagent/MCP server，不当后端。详见 [ADR-001](docs/production-design/adr-001-agent-backend-strategy.md)。

---

## 2. 核心目标（最重要）

> **如果用户按快捷键后不能立即输入，其他所有功能都没有意义。**

所有改动都应服务于这条主链路的可靠性：
`Alt+Space → 窗口出现 → 自动 Focus → 用户直接输入 → ESC/失焦隐藏`。

| 指标 | 目标 |
|---|---|
| 快捷键唤起延迟 | &lt; 50ms |
| 输入首个结果延迟 | &lt; 20ms |
| 常驻内存 | &lt; 300MB（Tauri + WebView2 基线约 80-150MB） |
| 输入焦点成功率 | &gt; 99.9% |

---

## 3. 技术栈与构建

| 层 | 技术 |
|---|---|
| 框架 | Tauri 2（Rust 后端 + WebView2 前端） |
| 后端 | Rust 2024、SQLite（`sqlx`）、`tokio`、`tracing` |
| 前端 | 纯静态 HTML/CSS/JS，**无 bundler、无 npm、无构建步骤** |
| 平台 | `windows` crate 直接调 Win32（热键 hook、窗口、Shell 图标、UIA） |

```bash
cargo tauri dev          # 开发（debug，控制台 tracing，默认 error 级；设置页可调）
cargo xtask release      # 打包（= 编译插件 + cargo tauri build；需先 cargo install tauri-cli）
cargo test --bin blink   # 跑单测（bin crate，无 lib target）
```

---

## 4. 关键业务决策（无法从代码推断）

这些是影响实现取舍的架构级约束：

| 决策 | 说明 |
|---|---|
| **热键默认不吞键** | hook 回调全程 `CallNextHookEx` 放行，Alt 仍可作系统修饰键。tap/hold 靠按压时长 + 期间是否出现其他键区分。**例外（0.10.7）**：chord 独占模式下，主窗 Alt hold 时吞 chord 键 keydown（仅字母键，不碰 Alt 本身），让 Blink 独占 chord 触发，避免与其他软件 Alt+A 等冲突。退出 chord mode 即恢复放行。 |
| **看门狗失焦检测** | 不依赖 `WM_ACTIVATE`，每 150ms 轮询 `GetForegroundWindow()`，按**进程 PID** 判定（非死比 HWND）。 |
| **搜索双路匹配** | 同时对原始名和拼音首字母做 nucleo fuzzy 取最高分；历史 `ln(hit+1)*0.3` 加权（上限 0.8）。 |
| **图标懒加载** | 图标提取**不进搜索热路径**，由自定义协议 `blink-icon` 按需提供。 |
| **lnk_path 是 history 主键** | 扫描产生的路径字符串不可随意归一化/改写，否则历史权重 key 失配。 |

---

## 5. 模块拆分速查

**目录结构（域驱动）**：
- `src/main.rs` — Tauri 启动 + 托盘 + 各服务 wiring
- `src/app/` — 应用层：commands（Tauri IPC 入口）、config、ai_config（第 7 分片）、stt_config（第 8 分片）、voice（语音管线编排）
- `src/domain/` — 业务域（四域架构，见 §9）：
  - `context/` — Awareness：`is_url` / `is_file_path` / `AwarenessSnapshot`
  - `intent/` — Suggestion + Routing：`RuleRouter` / `Suggestion` / `ExecArg` / `RankingHint` / `SuggestionProducer` / `SuggestionArbiter`
  - `search/` — SearchService + SearchEngine trait + 各引擎（builtin / calc / clipboard / file / start_menu）
  - `execution/` — `Action` trait + `ActionOutcome` + `ActionContext` + `ActionSchema` + `DangerClass` + `ActionRegistry`（12 builtin + 3 chord）
  - `plugin/` — manifest 解析 + JSONL 协议 + tokio 子进程
  - `chord/` — `ChordAction` trait + `ChordRegistry`
  - `ai/` — `AIProvider` trait + `AIProviderRegistry` + `RigFactory` + `gating`（四筛子）+ `message`（ChatMessage / ToolCall）+ `rig_provider` + `memory`（SqliteConversationMemory impl rig ConversationMemory）+ `tool_adapter`（CapabilityTool/ActionTool impl ToolDyn，对话窗口 tool 池适配层）
  - `stt/` — `SttEngine` trait + 模型注册表 + cloud / local / pseudo_streaming（伪流式 VAD+预览）/ vad / funasr（服务生命周期）/ wav
  - `capability/` — `Capability` trait + `InvokeContext` + `CapabilitySchema` + `CapabilityResult` + `CapabilityError` + `CapabilityRegistry`（inventory 自动注册）+ `builtins/`（capture_screen / crop_image / read_clipboard / write_clipboard / search_files）
- `src/infra/` — 基础设施层：
  - `platform/` — `mod.rs` 抽象 + `windows.rs`：hotkey / window / selection / clipboard / context / locale / screenshot / secret（Credential Manager）/ python（uv 自管理 Python 环境）/ audio（cpal 麦克风采集）/ inject（文本注入 SendInput+Clipboard 两级回退）
  - `data/` — SQLite：history / clipboard / config KV（`AppConfig` 6 分片 + `AIConfig` 第 7 分片 + `SttConfig` 第 8 分片）
  - `utils/` — logging / perf（SLO）/ text（拼音）

### 前端（`frontend/`）

- 主窗口：`index.html` + `style.css` + `js/*.js`（搜索/结果/键盘/动作/生命周期/主题/i18n/Ghost/Chord）
- 设置页：`settings.html` + `settings.js` + `settings.css`（含语音 Tab：`js/settings/tabs/voice.js` + `css/views/settings-voice.css`）
- 对话窗口：`chat.html` + `js/chat/*.js`（main/state/ipc/renderer/components/composer/sidebar）+ `css/views/chat.css`（流式 Markdown / 模型选择器 / 思考块 / 语音输入 / 多对话侧边栏）
- 悬浮球：`chord-ball.html`
- 截图 overlay：`chord-screenshot.html`
- 语音 overlay：`voice-overlay`（G2 语音输入法上屏 mini 窗口）
- 右键菜单：`contextmenu-popup.html`

前端用 `invoke()` 调 Rust commands，用 `TAU.event.listen()` 监听后端事件（`blink://shown`/`hidden`/`results`/`chord-translate`/`chord-fill-query`/`ai-confirm-action`/`voice-partial`/`audio-test-level`/`funasr-server-status`/`funasr-server-log`/`python-env-progress`）。

---

## 6. 编码约定

| 规则                | 说明                                                                          |
|-------------------|-----------------------------------------------------------------------------|
| **配置化优先**         | 可选行为（默认值用户可能想改的）做成配置项 + 合理默认；纯内部参数不暴露。                                      |
| **关键节点打印日志**     | 关键的节点需要打日志，量要适中且等级合适，开发流程也可以打一些临时日志用来排查问题，但在收尾时要注意清理                        |
| **统一 tracing 日志** | 禁止散落 `println!/eprintln!`；error=异常、warn=潜在问题、info=状态变化、debug=主流程、trace=诊断细节 |
| **结构化日志**         | `tracing::debug!(%query, "搜索")` 而非字符串拼接；错误必带上下文 `(%path, %e)`               |
| **改完自审**          | 每次完成改动后自己 review（diff / 编译 / 副作用）再报告                                        |
| **平台抽象预留**        | 平台相关逻辑走 `mod.rs` 接口 + `windows.rs` 实现                                       |
| **不过度工程**         | 0.x 阶段不对外发布，产品化基础设施（manifest 升级/权限强制/插件市场）1.0 前不做                           |
| **架构要有前瞻性**       | 精心设计持续演进，不过早腐败，不随便堆砌坏味道与技术债，持续收敛，Clean Architecture                         |

---

## 7. 测试策略（务实 TDD）

- ✅ **纯逻辑/算法必须有单测**：计算、fuzzy/拼音、PNG 编码、状态机等可纯函数化的逻辑。主动把可测逻辑从平台调用里抽出来。
- ❌ **Win32/GUI/Shell/Tauri 集成层免自动化**：这类调用难以稳定 mock，靠 `cargo run` 手动验证主链路。
- ⚠️ **依赖系统资源的测试要可跳过**：用 `Path::exists` 守卫，缺失则跳过（不依赖 CI 桌面环境）。
- ✅ **验证产物正确性**：例如断言 PNG 魔数，而不只是 `!is_empty()`。

---

## 8. 数据存储

> **0.12.0 已落地：DB 四层拆分**（`DbPools` struct 持有四个独立 `SqlitePool`，独立写锁互不阻塞）

SQLite `%APPDATA%\blink\`（四库独立）：
- **配置库 `blink_config.db`** — `config(key, value, updated_at)` 配置 KV（`AppConfig` 6 分片门面 + `AIConfig` 第 7 分片 + `SttConfig` 第 8 分片 + `engine:{id}` / `plugin:{id}` / `clipboard:config` / `screenshot:config` / `context:config`）；未来跨机同步只同步此库
- **历史库 `blink_history.db`** — `history(lnk_path, hit_count, last_used_at)` 启动历史 + `clipboard_history(id, text, kind, hit_count, last_used_at)` 剪贴板历史
- **AI 库 `blink_ai.db`** — `ai_tool_audit` AI 工具审计（0.12.0 加 `cleanup_old` 30 天 + 行数上限 10000）+ `conversations` / `messages`（0.12.3 对话记忆持久化）
- **缓存库 `blink_cache.db`** — `performance_metrics` 性能统计（高频写）+ `icon_cache` 图标缓存（BLOB）

文件系统 `%APPDATA%\blink\`：
- `python\uv\uv.exe` — uv 二进制（本地安装，Blink 自管理）
- `python\venv\` — Python 3.12 虚拟环境（uv 创建，funasr 等包安装于此）

---

## 9. 四域架构（0.8.4 起为设计基座）

**必读**：[docs/production-design/phases/0.8-context-interaction.md §五](docs/production-design/phases/0.8-context-interaction.md)（四域重构）+ [§八](docs/production-design/phases/0.8-context-interaction.md#八08.6-规划架构固化为-0.9-铺物理骨架)（0.8.6 架构固化）。

```
Awareness (环境感知)     — 抓 snapshot,纯数据不做判断
    ↓  唯一读它的层
Suggestion (建议生产)    — Signal → Ghost 建议,待用户采纳
    ↓  ★ 信任边界:Tab/点击/打字才穿过
Routing    (路由决策)    — Query → Route,对 Awareness 无知
    ↓  只有用户显式选择才穿过
Execution  (执行)        — UserExplicit 参数才真执行
```

**三条铁则**（架构强制，类型系统钉死）：
1. **呈现权 ≠ 执行权**：Routing 只出候选，Execution 需第二次交互
2. **参数注入必须显式**：`ExecArg::UserExplicit(String)` 类型墙
3. **弱信号 pull 不 push**：Routing 无法读 Awareness，Context 只能通过 Suggestion 域影响

**三个统一入口**：
- `Action` trait（`domain/execution/`）—— 一切副作用的统一入口
- `SuggestionProducer` + `SuggestionArbiter`（`domain/intent/suggestion/`）—— 一切建议的统一入口（Keyword / Context / AI 三源竞争）
- `ConfigStore<T>` + 前端 `set_config` —— 一切配置的统一入口

