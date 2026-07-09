# CLAUDE.md

本文件为 Claude Code 在本仓库工作时提供指引。**指令优先级高于默认行为，必须严格遵循。**

> 📖 **产品设计与文档导航**：请先阅读 [docs/production-design/00-overview.md](docs/production-design/00-overview.md) 了解产品定位、里程碑与完整文档体系。改核心前必读对应 phases 文档。

更新时间 20260709

---

## 1. 项目概览

Blink 是一个 Windows 全局快捷入口，定位不是「启动器」，而是 **Universal Action Layer（统一操作层）**。
终极目标：感知用户上下文、主动推荐动作，让任何操作都比原来的路径更快。

当前处于 **0.9 进行中**（0.9.0~0.9.3 已完成）。详见 [phases/0.9-ai-layer.md](docs/production-design/phases/0.9-ai-layer.md)。

- ✅ **0.9.3 插件 tool-call 支持**：AI 路由能调用插件声明的 tool，ActionRegistry 改 RwLock 支持动态注册。详见 [0.9-ai-layer.md §四](docs/production-design/phases/0.9-ai-layer.md#四插件-tool-call-支持093)
- 🔜 **0.10 语音指令闭环**：STT + 双 chord 语音入口 + 语音找文件 + Agent 对话窗口。详见 [phases/0.10-voice-agent.md](docs/production-design/phases/0.10-voice-agent.md)
- 🔜 **0.11 本地化与生态**：本地模型 / skill / MCP / RAG。详见 [phases/0.11-local-ecosystem.md](docs/production-design/phases/0.11-local-ecosystem.md)

---

## 2. 核心目标（最重要）

> **如果用户按快捷键后不能立即输入，其他所有功能都没有意义。**

所有改动都应服务于这条主链路的可靠性：
`右 Alt 单击 → 窗口出现 → 自动 Focus → 用户直接输入 → ESC/失焦隐藏`。

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
| **热键不吞键** | hook 回调全程 `CallNextHookEx` 放行，右 Alt 仍可作系统修饰键。tap/hold 靠按压时长 + 期间是否出现其他键区分。 |
| **看门狗失焦检测** | 不依赖 `WM_ACTIVATE`，每 150ms 轮询 `GetForegroundWindow()`，按**进程 PID** 判定（非死比 HWND）。 |
| **搜索双路匹配** | 同时对原始名和拼音首字母做 nucleo fuzzy 取最高分；历史 `ln(hit+1)*0.3` 加权（上限 0.8）。 |
| **图标懒加载** | 图标提取**不进搜索热路径**，由自定义协议 `blink-icon` 按需提供。 |
| **lnk_path 是 history 主键** | 扫描产生的路径字符串不可随意归一化/改写，否则历史权重 key 失配。 |

---

## 5. 模块拆分速查

**目录结构（域驱动）**：
- `src/main.rs` — Tauri 启动 + 托盘 + 各服务 wiring
- `src/app/` — 应用层：commands（Tauri IPC 入口）、config、ai_config（第 7 分片）
- `src/domain/` — 业务域（四域架构，见 §9）：
  - `context/` — Awareness：`is_url` / `is_file_path` / `AwarenessSnapshot`
  - `intent/` — Suggestion + Routing：`RuleRouter` / `Suggestion` / `ExecArg` / `RankingHint` / `SuggestionProducer` / `SuggestionArbiter`
  - `search/` — SearchService + SearchEngine trait + 各引擎（builtin / calc / clipboard / file / start_menu）
  - `execution/` — `Action` trait + `ActionOutcome` + `ActionContext` + `ActionSchema` + `DangerClass` + `ActionRegistry`（12 builtin + 3 chord）
  - `plugin/` — manifest 解析 + JSONL 协议 + tokio 子进程
  - `chord/` — `ChordAction` trait + `ChordRegistry`
  - `ai/` — `AIProvider` trait + `AIProviderRegistry` + `RigFactory` + `gating`（四筛子）+ `message`（ChatMessage / ToolCall）+ `rig_provider`
- `src/infra/` — 基础设施层：
  - `platform/` — `mod.rs` 抽象 + `windows.rs`：hotkey / window / selection / clipboard / context / locale / screenshot / secret（Credential Manager）
  - `data/` — SQLite：history / clipboard / config KV（`ConfigStore<T>` 6 分片）
  - `utils/` — logging / perf（SLO）/ text（拼音）

### 前端（`frontend/`）

- 主窗口：`index.html` + `style.css` + `js/*.js`（搜索/结果/键盘/动作/生命周期/主题/i18n/Ghost/Chord）
- 设置页：`settings.html` + `settings.js` + `settings.css`
- 悬浮球：`chord-ball.html`
- 截图 overlay：`chord-screenshot.html`
- 右键菜单：`contextmenu-popup.html`

前端用 `invoke()` 调 Rust commands，用 `TAU.event.listen()` 监听后端事件（`blink://shown`/`hidden`/`results`/`chord-translate`/`chord-fill-query`/`ai-confirm-action`）。

---

## 6. 编码约定

| 规则                     | 说明                                                                          |
|------------------------|-----------------------------------------------------------------------------|
| **配置化优先**              | 可选行为（默认值用户可能想改的）做成配置项 + 合理默认；纯内部参数不暴露。                                      |
| **统一 tracing 日志**      | 禁止散落 `println!/eprintln!`；error=异常、warn=潜在问题、info=状态变化、debug=主流程、trace=诊断细节 |
| **结构化日志**              | `tracing::debug!(%query, "搜索")` 而非字符串拼接；错误必带上下文 `(%path, %e)`               |
| **改完自审**               | 每次完成改动后自己 review（diff / 编译 / 副作用）再报告                                        |
| **平台抽象预留**             | 平台相关逻辑走 `mod.rs` 接口 + `windows.rs` 实现                                       |
| **不过度工程**              | 0.x 阶段不对外发布，产品化基础设施（manifest 升级/权限强制/插件市场）1.0 前不做                           |
| **架构要有前瞻性**           | 精心设计持续演进，不过早腐败，不随便堆砌坏味道与技术债，持续收敛，Clean Architecture                         |

---

## 7. 测试策略（务实 TDD）

- ✅ **纯逻辑/算法必须有单测**：计算、fuzzy/拼音、PNG 编码、状态机等可纯函数化的逻辑。主动把可测逻辑从平台调用里抽出来。
- ❌ **Win32/GUI/Shell/Tauri 集成层免自动化**：这类调用难以稳定 mock，靠 `cargo run` 手动验证主链路。
- ⚠️ **依赖系统资源的测试要可跳过**：用 `Path::exists` 守卫，缺失则跳过（不依赖 CI 桌面环境）。
- ✅ **验证产物正确性**：例如断言 PNG 魔数，而不只是 `!is_empty()`。

---

## 8. 数据存储

SQLite `%APPDATA%\blink\blink.db`：
- `history(lnk_path, hit_count, last_used_at)` — 启动历史，频率加权 + 衰减
- `config(key, value, updated_at)` — 配置 KV（`AppConfig` 6 分片门面 + `AIConfig` 第 7 分片 + `engine:{id}` / `plugin:{id}`）；前端泛型 `set_config` 命令
- `clipboard(id, text, kind, hit_count, last_used_at)` — 剪贴板历史
- `perf(metric, value, at)` — 性能统计

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

