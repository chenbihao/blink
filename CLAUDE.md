# CLAUDE.md

本文件为 Claude Code 在本仓库工作时提供指引。**指令优先级高于默认行为，必须严格遵循。**

> 📖 **产品设计与文档导航**：请先阅读 [docs/production-design/00-overview.md](docs/production-design/00-overview.md) 了解产品定位、里程碑与完整文档体系。改核心前必读对应 phases 文档。

更新时间 20260629

---

## 1. 项目概览

Blink 是一个 Windows 全局快捷入口，定位不是「启动器」，而是 **Universal Action Layer（统一操作层）**。
终极目标：感知用户上下文、主动推荐动作，让任何操作都比原来的路径更快。

当前处于 **0.8 进行中**：0.1~0.7 全部完成，0.8 正在进行 UIA 划词监听 + Chord 模式 + AI 能力基础。

**最新特性（0.8）**：
- ✅ UIA 划词文本感知（鼠标选中文本自动抓取）
- ✅ 内置动作抽象升级：`SearchAction::RunAction` + Context 感知（剪贴板 URL/文件路径触发"打开链接/打开路径/资源管理器定位"）+ 设置页 disable 面板 + 拼音全拼匹配
- 📋 Chord 模式：按住 Alt + 二次快捷键直接触发动作（截图/划词/剪贴板）
- 🔜 AI Provider 抽象 + 云端插件（OpenAI 兼容）

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

平台相关模块统一拆为 `mod.rs`（接口 + 通用逻辑）+ `windows.rs`（Windows 实现）。

### Rust 后端（`src/`）

| 模块 | 核心职责 |
|---|---|
| `main.rs` | Tauri 初始化、托盘、tracing、自定义协议注册、启动各后台任务 |
| `commands.rs` | Tauri command 层 — 前端 `invoke()` 入口，轻量编排 |
| `config.rs` | `AppConfig`（快捷键/tap阈值/grace/自启/语言/日志级别/主题/代理等）SQLite 持久化 + 热更新 |
| `hotkey/` | 全局热键：`WH_KEYBOARD_LL` + 物理态查询状态机 + 录制 |
| `window/` | 窗口控制：显隐、看门狗失焦轮询、显示器定位 |
| `search/` | 同步/异步双 lane 搜索 + 引擎抽象 + 开始菜单/文件/计算/内置/插件引擎 + 图标提取 |
| `plugin/` | 插件系统：manifest 解析 + JSONL 协议 + tokio 子进程（Process/Python/Node/Powershell） |
| `context/` | Context 层：环境感知、敏感应用黑名单（采集实现在 `infra/platform/context/`；纯逻辑判定 `is_url` / `is_file_path` 在 `domain/context/probe.rs`）|
| `selection/` | **0.8 新增**：UIA 划词监听 + 文本抓取 + 缓存 |
| `intent.rs` | 意图引擎 RuleRouter：关键字→动作路由 |
| `clipboard.rs` | 剪贴板历史：监听/存储/搜索/管理 |
| `perf.rs` | 性能统计：SQLite 指标存储 + RAII Timer + 百分位查询 |
| `service.rs` | Service 骨架：`Service` trait + `AppContext` + 生命周期统一编排 |
| `history.rs` | SQLite 历史记录（频率加权 + 衰减）+ `config` 表 KV 读写 |

### 前端（`frontend/`）

- 主窗口：`index.html` + `style.css` + `js/*.js`（搜索/结果/键盘/动作/生命周期/主题/i18n）
- 设置页：`settings.html` + `settings.js` + `settings.css`（通用/快捷键/引擎/插件/网络/上下文/存储/调试）
- 右键菜单：`contextmenu-popup.html`

前端用 `invoke()` 调 Rust commands，用 `TAU.event.listen()` 监听后端事件（`blink://shown`/`hidden`/`results`）。

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
- `config(key, value, updated_at)` — 配置 KV（分命名空间：`app_config` / `engine:{id}` / `plugin:{id}` / `context:{key}`）
