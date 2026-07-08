# CLAUDE.md

本文件为 Claude Code 在本仓库工作时提供指引。**指令优先级高于默认行为，必须严格遵循。**

> 📖 **产品设计与文档导航**：请先阅读 [docs/production-design/00-overview.md](docs/production-design/00-overview.md) 了解产品定位、里程碑与完整文档体系。改核心前必读对应 phases 文档。

更新时间 20260708

---

## 1. 项目概览

Blink 是一个 Windows 全局快捷入口，定位不是「启动器」，而是 **Universal Action Layer（统一操作层）**。
终极目标：感知用户上下文、主动推荐动作，让任何操作都比原来的路径更快。

当前处于 **0.9.1 AI 地基完成——进入 0.9.2 应用闭环阶段**：0.1~0.7 全部完成；0.8.0 ~ 0.8.8 全部落地；0.9.0 依赖前置 + Action trait tool-call 进化完成；**0.9.1 AI 地基全部落地**（`AIProvider` trait 类型收窄 + SecretString + Credential Manager 密钥层 + AIConfig 第 7 分片 + 空档降级 + `AIProviderRegistry` + `ProviderFactory` trait + 切换零重启 + 前端 AI Tab 完整 UX）。**测试基线 380 通过，零 warning**。AI 相关能力（真 rig client / 语音 / 生态）规划在 0.9.2 ~ 0.11 三步走（详见 [phases/0.9-ai-layer.md](docs/production-design/phases/0.9-ai-layer.md)）。

**最新特性（0.8）**：
- ✅ **0.8.0** UIA 划词文本感知（鼠标选中文本自动抓取）
- ✅ **0.8.0** 内置动作抽象升级：`SearchAction::RunAction` + Context 感知（剪贴板 URL/文件路径触发"打开链接/打开路径/资源管理器定位"）+ 设置页 disable 面板 + 拼音全拼匹配
- ✅ **0.8.1** Autosuggestion / Ghost Text：首拼命中降级 Priority + 灰色行内补全 + Tab 显式升级；suggest fuzzy 覆盖中文原文与 pinyin_full 双候选；插件 manifest 支持 `empty_arg_hint`（空 arg 命中直接展示静态提示，跳过进程/IPC 调用）
- ✅ **0.8.2** 翻译插件 Context 感知路由：选中/剪贴板非目标语言 → 翻译插件被路由（`needs_translation` + `PluginSettingResolver` trait；URL/文件路径护栏内建）。**注**：架构层已完成，UX 层的 push 模式过于激进 → 0.8.3 转 Ghost
- ✅ **0.8.3** 感知交互统一：`Suggestion` 契约抽象（合并 completion_hint + context_suggestion，为 0.9 AI 铺路）+ Context 转 Ghost + Tab 采纳 + 上下文感知面板 + Ghost 本地化 display + origin 来源提示（来自划词/剪贴板）+ **awareness 域重构**（`AwarenessSnapshot { texts: Vec<AwarenessText { source, text }> }` 数据侧带 origin,intent 层零推断,删除 3 个推断 helper）
- ✅ **0.8.4** **四域架构重构**：Awareness / Suggestion / Routing / Execution 四域强边界（`route` 断 Awareness）+ `ExecArg` 类型墙 + RankingHint Surface Booster + Suggestion 覆盖非空 query + 内置动作保持「展示即抽参」（不延后，详见 phases §5.1）+ PluginProtocol.clipboard_text deprecated
- ✅ **0.8.5** **Chord 交互底层**：Ghost overlay `.ghost-chord` 影子层提示（`:has()` 让位 Ghost 补全）+ 独立悬浮球 `chord-ball` webview（`WS_EX_NOACTIVATE` 不抢焦点）+ Alt+Q 划词 confirm flow + Alt+C 剪贴板走 `Route::EngineTakeover`（新增 `SearchEngine::takeover_only()` trait + 独立 `ClipboardEngine`）+ 剪贴板监听器补 0.7 漏 + 设置页 Chord tab + `ChordAction::label` 走 `LocalizableText`
- ✅ **0.8.6** **架构固化全部完成**（为 0.9 铺物理骨架，纯横向重构不动业务）：Action trait 收敛完成（`SearchAction/BuiltinActionKind/PluginAction/ChordAction` 统一走 `Action` trait + `ActionOutcome`，位于 `src/domain/execution/`）+ Suggestion Producer/Arbiter 完成（`src/domain/intent/suggestion/`）+ AppContext 真依赖容器完成 + `PluginQueryContext.clipboard_text` 已删 + 内置动作 title/subtitle i18n 完成 + **ConfigStore 6 分片后端完成** + **前端 IPC 泛型化完成**（20+ `update_*` 命令收敛为 1 个 `set_config` 泛型命令 + `frontend/js/config-keys.js`）+ SearchService God Method 已拆为 `exec_takeover` / `exec_engine_takeover` / `exec_mixed`。测试基线 309 通过
- ✅ **0.8.7** **Alt+A 区域截图**：Chord 三剑客补齐；`src/infra/platform/screenshot/` 新增 GDI 截屏模块 + `SESSION` 单点缓存 + `crop_rgba` 纯函数（7 个单测）；DWM Cloak 隐藏主窗绕过 Win11 fade 动画；BGRA 全链路（BitBlt→SESSION→CF_DIB 免全屏 swap）；PNG 编码 `Compression::Fast + FilterType::NoFilter + u32 位运算 swap + dev profile 局部 opt=3` 从 600ms → ~150ms；总感知延迟 ~320ms（dev），release 更快
- ✅ **0.8.8** **收尾归档**：0.8.6 落地盘点补入文档 §8.7 + 里程碑表状态同步 + `.taurignore` 落地（改 md 不触发 rebuild）+ 冗余清理小刀（删 `icon::clear_cache` / `update_interpreter_config` / `text::normalize_candidates` 三处僵尸，净减 54 行）+ **设计 token 层落地**（`theme.css` 新增 `--radius-sm/md/lg/xl` + `--transition-fast/base/slow`，`settings.css` ~85% hardcode 迁 token，铁则写入 principles §14.8）+ 版本号统一到 0.8.8 + **P1-C ConfigStore 6 分片后端 + 前端 IPC 泛型化**（20+ `update_*` → 1 个 `set_config` 命令 + `config-keys.js`）+ **P2-A SearchService 拆分**（God Method → `exec_takeover` / `exec_engine_takeover` / `exec_mixed`）。测试基线 309 通过
- ✅ **0.9.0** **依赖前置 + Action trait tool-call 进化**（纯架构零 AI）：升 tauri 2.11.3→2.11.5 + reqwest 0.12→0.13 + tauri-plugin-autostart 2.4→2.5.1 + tauri-plugin-dialog 2.2→2.7.1；引入 rig-core 0.39（features: rmcp,default 已含 derive+reqwest+rustls）；新增 `src/domain/execution/schema.rs`（`ActionSchema` + `DangerClass` + `to_rig_tool()` rig 类型投影）；`Action` trait 加 `schema()` + `danger_class()` default impl；`ActionContext { arg: Option<Value> }` → `{ arguments: Value }` + `_legacy_arg` 兼容层 + `from_arguments()` 防御性入口；12 个 builtin **全部显式**覆盖 `schema()` + `danger_class()`（6 Safe + 6 Dangerous 分类：`open_settings/open_logs/open_data_dir/open_url/open_path/reveal_in_explorer` 为 Safe,`lock/shutdown/restart/sleep/clear_history/exit_blink` 为 Dangerous）；3 个 Chord 动作显式覆盖；参数化 3 个动作换语义键（`url`/`path`）+ 保留 `_legacy_arg` fallback；`src/infra/utils/perf.rs::ai_slo` mod 定义 SLO tracing target 骨架（`blink::ai::slo` + `first_token_ms/intent_total_ms/fallback_to_search/timeout/tier` 字段常量）。测试基线 **327 通过零 warning**（+15：5 schema + 8 execution + 2 registry 元测试）
- ✅ **0.9.1** **AI 地基**（Provider trait + 密钥安全 + 配置页三档 + AIConfig 分片,不含真 rig 调用）：
  - **抗延迟骨架 spike**（`src/domain/ai/spike/`,`#[cfg(test)]` gated）：mock TCP server + 硬超时精确 abort + `tokio::select!` 中断 + `AbortHandle` 取消 + loading 150ms 反闪烁 + SecretString 生命周期干净——4/7 骨架条走通,剩 3 条（SSE first_token_ms/rig Client 冷构造/Provider 切换零重启）并入后续 Phase
  - **密钥存储层**（`src/infra/platform/secret/`）：`SecretString` newtype 包 `Zeroizing<String>`,drop 按字节清零;Debug/Display 双通路掩码（`<redacted>` / `••••{last4}`）;不实现 Serialize;`build_target_name` 拒非法字符;Windows Credential Manager 三链路 `save_secret / load_secret / delete_secret`
  - **AIConfig 第 7 分片**（`src/app/ai_config.rs`,独立于 AppConfig 门面）：默认 `enabled=false` 老用户零副作用;`secret_ref` 而非 raw Key 序列化;`resolve_tier` 空档降级三链（Router→Light→Main→None）+ 悬空引用降级;`tiers_referencing` 为 §6.4 UX 铺底;`ProviderKind` 显式 rename 避 `open_a_i` 灾难
  - **AIProvider trait**（`src/domain/ai/provider.rs`,§2.6 类型收窄编译期钉死）：trait 表面只有 `kind / model_id / complete`,**无 `agent_session / prompt / memory`**;`AIError` 不含供应商原始明细;`MockProvider` 用于 dispatch 测试
  - **AIProviderRegistry**（`src/domain/ai/registry.rs` + `factory.rs`）：`ProviderFactory` trait 抽象;池增量热更新（未变动 (pid, mid) 复用旧 Arc,骨架条 #7 实机验证）;`NoopFactory` 占位（Phase 5b 起换 `RigFactory`）;factory 失败单个 skip 不影响其他
  - **前端 AI Tab**（`frontend/settings.html` + `settings.js` + `settings-modal.css` + `i18n.js` zh+en）：总开关（§5.3 严格 opt-in toast）/ 供应商列表 / 添加供应商 modal（Kind + display_name + base_url + API Key + models）/ 三档下拉 + 即时降级提示 + 顶部 banner（§6.4 UX 埋点四处）/ 未命中过滤四筛子 / 高级（Safe 白名单 + 硬超时);数字输入用共享 `.number-input-wrapper`
  - **命令新增**:`save_ai_secret / delete_ai_secret / has_ai_secret`（明文只在参数一次窗口存活,never 回传;失败回滚 CM 密钥防悬空）;`set_config('ai_config')` 分派 → registry.reload → **切换零重启在真前端触发下走通**
  - **AppContext 挂载**:`ai_config` + `ai_registry` 字段;main.rs setup 读 AIConfig + 构造 registry + 挂 Tauri state
  - 测试基线 **380 通过零 warning**（+53:5 spike skeleton + 5 secret hygiene + 12 secret backend + 13 ai_config + 8 provider + 8 registry + 2 factory)
  - **未完成部分**:Phase 5b（真 rig Client 落地 `RigFactory` 替换 `NoopFactory`）并入 0.9.2 SearchService 一起做——那时能"配 Provider → 触发 completion → 观察 SLO"端到端验证
- 🔜 **0.9.2** 主窗口文本闭环 + 轻量路由 + 反馈动画 + SLO 埋点上数据 + 未命中过滤四筛子 spike 阈值 + **Phase 5b 真 rig Client 落地**（RigFactory 替换 NoopFactory）。详见 [phases/0.9-ai-layer.md](docs/production-design/phases/0.9-ai-layer.md)
- 🔜 **0.10 语音指令闭环**：STT + 双 chord 语音入口 + 语音找文件北极星场景 + Agent 对话窗口。架构不变只加感知层。详见 [phases/0.10-voice-agent.md](docs/production-design/phases/0.10-voice-agent.md)
- 🔜 **0.11 本地化与生态**：本地模型按需下载 / skill 化 / MCP 双向 / RAG 记忆（按需增强）。详见 [phases/0.11-local-ecosystem.md](docs/production-design/phases/0.11-local-ecosystem.md)

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

**目录结构（0.8.4 起域驱动）**：
- `src/main.rs` — Tauri 启动 + 托盘 + 各服务 wiring
- `src/app/` — 应用层：commands（Tauri IPC 入口）、config、service（生命周期骨架）
- `src/domain/` — 业务域（四域架构，见 phases/0.8 §5）：
  - `context/` — Awareness 域纯逻辑（`is_url` / `is_file_path` / `AwarenessSnapshot`；采集实现在 `infra/platform/context/`）
  - `intent/` — Suggestion 域 + Routing 域：`RuleRouter` / `Suggestion` / `ExecArg` / `RankingHint`
  - `search/` — Query 域：`SearchService` + `SearchEngine` trait + 各引擎（builtin / calc / clipboard / file / start_menu）
  - `plugin/` — 插件系统：manifest 解析 + JSONL 协议 + tokio 子进程
  - `chord/` — Chord 交互：`ChordAction` trait + `ChordRegistry`
  - `ai/`（🔜 0.9 规划）— AI 域：Provider trait（多档：路由/轻量/主，包 rig-core `CompletionModel`）+ 注册表 + ChatMessage + ToolCall 分派。详见 [phases/0.9-ai-layer.md](docs/production-design/phases/0.9-ai-layer.md)
- `src/infra/` — 基础设施层：
  - `platform/` — 平台相关（`mod.rs` 抽象 + `windows.rs` 实现）：hotkey / window / selection / clipboard / context / locale / screenshot
  - `data/` — SQLite 持久化：history / clipboard / config KV
  - `utils/` — 通用工具：logging / perf / text（拼音）

**0.8.6 架构固化落地**：
- `src/domain/execution/` — Execution 域物理落地:`Action` trait（0.9.0 tool-call 进化,加 `schema()` + `danger_class()`）+ `ActionOutcome` + `ActionContext.arguments` 结构化 + Builtin/Chord 显式实现 + registry ✅（Plugin adapter 0.9.1 与 AI 一起做）
- `src/domain/intent/suggestion/` — Suggestion 域拆分:`SuggestionProducer` trait + `SuggestionArbiter` + Keyword/Context 两个 producer ✅
- `ConfigStore<T>` 分片:抽象层 ✅ + `AppConfig` 6 分片后端 ✅ + **前端 IPC 泛型化 ✅**（20+ `update_*` → 1 个 `set_config` 命令 + `frontend/js/config-keys.js`）

### 前端（`frontend/`）

- 主窗口：`index.html` + `style.css` + `js/*.js`（搜索/结果/键盘/动作/生命周期/主题/i18n/Ghost/Chord）
- 设置页：`settings.html` + `settings.js` + `settings.css`（通用/快捷键/引擎/插件/网络/上下文/Chord 交互/存储/调试）
- 悬浮球：`chord-ball.html`（0.8.5 新）
- 截图 overlay：`chord-screenshot.html`（0.8.7 新）
- 右键菜单：`contextmenu-popup.html`

前端用 `invoke()` 调 Rust commands，用 `TAU.event.listen()` 监听后端事件（`blink://shown`/`hidden`/`results`/`chord-translate`/`chord-fill-query`）。

---

## 6. 编码约定

| 规则 | 说明 |
|---|---|
| **配置化优先** | 可选行为（默认值用户可能想改的）做成配置项 + 合理默认；纯内部参数不暴露。 |
| **统一 tracing 日志** | 禁止散落 `println!/eprintln!`；error=异常、warn=潜在问题、info=状态变化、debug=主流程、trace=诊断细节 |
| **结构化日志** | `tracing::debug!(%query, "搜索")` 而非字符串拼接；错误必带上下文 `(%path, %e)` |
| **改完自审** | 每次完成改动后自己 review（diff / 编译 / 副作用）再报告 |
| **平台抽象预留** | 平台相关逻辑走 `mod.rs` 接口 + `windows.rs` 实现 |
| **不过度工程** | 0.x 阶段不对外发布，产品化基础设施（manifest 升级/权限强制/插件市场）1.0 前不做 |

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
- `config(key, value, updated_at)` — 配置 KV(分命名空间:`app.hotkey / app.appearance / app.search / app.suggestion / app.chord / app.disable` 六分片 + `engine:{id}` / `plugin:{id}` / `context:*` / `clipboard:config`;**0.8.8 已落地**——`AppConfig` 门面 struct 内部走 6 分片,老 `app_config` 单 key 自动迁移;20+ `update_*` 命令→泛型 `set_config<K>` 收敛留 0.9 起步和 AI Provider 一起做)
- `clipboard(id, text, kind, hit_count, last_used_at)` — 剪贴板历史（0.7 + 0.8.5 补监听）
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

**0.8.6 架构固化的三个统一入口**（0.9 AI 的物理骨架）：
- `Action` trait（`domain/execution/`）—— 一切副作用的统一入口;四份动作枚举 → 一个 trait + `ActionOutcome` ✅
- `SuggestionProducer` trait + `SuggestionArbiter`（`domain/intent/suggestion/`）—— 一切建议的统一入口;Keyword/Context/(0.9)AI 三源竞争 ✅
- `ConfigStore<T>` 泛型 + `AppConfig` 6 分片后端 + 前端 `set_config` 泛型命令 ✅ —— 一切配置的统一入口;0.9 加 `AIConfig` 只需 `impl ConfigKey for AIConfig` + 前端 `saveConfig("ai_config", {...})`

