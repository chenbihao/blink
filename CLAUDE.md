# CLAUDE.md

本文件为 Claude Code 在本仓库工作时提供指引。**指令优先级高于默认行为，必须严格遵循。**

更新时间 20260628

## 1. 项目概览

Blink 是一个 Windows 全局快捷入口，定位不是「启动器」，而是「Universal Action Layer（统一操作层）」。
终极目标：感知用户上下文、主动推荐动作，让任何操作都比原来的路径更快。

当前处于 **0.7** 阶段：0.1~0.7 全部完成（基础交互 → 服务化 → 插件骨架 → 意图路由 → 配置扩展 → 插件打包脚本 → 插件生态+本地搜索），0.8 意图路由优化 + AI 能力基础规划中。

## 2. 核心目标（最重要）

> **如果用户按快捷键后不能立即输入，其他所有功能都没有意义。**

所有改动都应服务于这条主链路的可靠性：
`右 Alt 单击 → 窗口出现 → 自动 Focus → 用户直接输入 → ESC/失焦隐藏`。

性能目标：

| 指标 | 目标 |
|---|---|
| 快捷键唤起延迟 | < 50ms |
| 输入首个结果延迟 | < 20ms |
| 常驻内存 | < 300MB（Tauri + WebView2 基线约 80-150MB） |
| 输入焦点成功率 | > 99.9% |

## 3. 技术栈

- **框架**：Tauri 2（Rust 后端 + WebView2 前端）。
- **后端**：Rust（edition 2024），SQLite 持久化（`sqlx`），`tokio` 异步运行时，`tracing` 日志。
- **前端**：纯静态 HTML/CSS/JS，**无 bundler、无 npm、无构建步骤**。`tauri.conf.json` 的 `frontendDist` 直接指向 `frontend/`。
- **平台 API**：`windows` crate 直接调 Win32（热键 hook、窗口、Shell 图标等）。
- **环境要求**：Rust、MSVC Build Tools、WebView2 Runtime。

构建运行：

```bash
cargo run                # 开发（debug，控制台 tracing，默认 error 级；设置页可调 info/debug）
cargo xtask release      # 打包（= 编译插件 + cargo tauri build；需先 cargo install tauri-cli）
cargo test --bin blink   # 跑单测（bin crate，无 lib target）
```

## 4. Roadmap

| 版本 | 内容 | 状态 |
|---|---|---|
| **0.1** | P0 基础交互（热键/窗口/焦点/IME）+ P1 搜索（应用/计算/历史权重）+ 配置系统/设置页/热键录制 | ✅ |
| **0.2** | 搜索缓存、开机自启、tracing 日志、窗口定位、应用图标 + Service 骨架 + SearchService/SearchEngine trait + 多路搜索 | ✅ |
| **0.3** | 插件系统骨架（独立进程 + stdio JSON + manifest）+ 热键物理态重构 | ✅ |
| **0.4** | 意图引擎 RuleRouter + Context 层 + ip 插件 | ✅ |
| **0.5** | 配置架构统一 KV + FileEngine（Everything）+ 右键菜单 + 主题系统 + i18n | ✅ |
| **0.6** | 插件打包路径 + Python/Node.js 脚本插件支持 + 统一错误处理 | ✅ |
| **0.7** | 插件生态（翻译/剪贴板历史）+ 本地搜索 Fallback + 性能统计 + 图标/搜索缓存优化 + 历史权重衰减 | ✅ |
| **0.8** | 意图路由优化（模糊匹配/多语言/上下文感知）+ AI Provider 抽象 + 基础 AI 插件 | 📋 规划中 |

**改核心前必读 `production-design/phases/0.2-core-plugin-design.md`**（Service/SearchEngine/Plugin/Intent 架构设计）。
Roadmap 内容了解即可，**不要提前实现**。

## 5. 业务相关决策

这些是无法从代码直接推断、影响实现取舍的关键决策：

- **热键不吞键**：hook 回调全程 `CallNextHookEx` 放行，右 Alt 仍可作系统修饰键。tap/hold 靠按压时长（≤tap_threshold）+ 期间是否出现其他键区分。**架构级约束**——做得差的产品在 keydown 就吞掉右 Alt。
- **看门狗失焦检测**：不依赖 `WM_ACTIVATE`，每 150ms 轮询 `GetForegroundWindow()`。因为某些窗口（如 IDEA 终端子进程）不发失焦通知。invoke 后有 grace period 覆盖焦点抖动。
- **搜索双路匹配**：同时对原始名和拼音首字母做 nucleo fuzzy 取最高分；历史 `ln(hit+1)*0.3` 加权（上限 0.8，由 `scorer.rs` 统一管理）。
- **图标懒加载**：图标提取**不进搜索热路径**，由自定义协议 `blink-icon` 按需提供（见 §6）。
- **lnk_path 是 history 主键**：扫描产生的路径字符串不可随意归一化/改写，否则历史权重 key 失配。

## 6. 高层架构与模块拆分

平台相关模块拆为 `mod.rs`（平台接口 + 通用逻辑）+ `windows.rs`（Windows 实现），为多平台预留。

### Rust 后端 (`src/`)

| 文件 | 职责 |
|---|---|
| `main.rs` | Tauri 初始化、托盘、tracing、自定义协议注册、启动各后台任务 |
| `commands.rs` | Tauri command 层 — 前端 `invoke()` 入口，轻量编排 |
| `config.rs` | `AppConfig`（快捷键/tap阈值/grace/自启/语言/日志级别/主题/代理等）SQLite 持久化 + 运行时热更新 |
| `logging.rs` | tracing：文件每日轮转（保留 7 天）+ 控制台 + 动态级别 reload |
| `hotkey/` | 全局热键：`mod.rs`(运行时配置/事件) + `windows.rs`(WH_KEYBOARD_LL；**触发判定=物理态查询**，见下) + `recorder.rs`(录制状态机，独立路径) |
| `window/` | 窗口控制：显隐、看门狗失焦轮询（按进程 PID 判定，非死比 HWND）、显示器定位 |
| `search/` | `mod.rs`(AppEntry/fuzzy/拼音) + `engine.rs`(SearchEngine trait/SearchItem) + `service.rs`(SearchService：sync/async 双 lane + 融合) + `start_menu_engine.rs`(开始菜单扫描+缓存) + `file_engine.rs`(Everything/本地 Fallback) + `calc_engine.rs` + `builtin_engine.rs`(内置功能引擎) + `mock_slow_engine.rs` + `scorer.rs`(统一评分公式) + `windows.rs`(扫描 .lnk) + `icon.rs`(图标提取+持久化缓存) |
| `plugin/` | 插件系统：`manifest.rs`(JSON manifest 解析) + `protocol.rs`(JSONL 协议) + `process.rs`(tokio 子进程管理，支持 Process/Python/Node/Powershell) + `engine.rs`(PluginEngine 接 async lane) |
| `context/` | Context 层（0.4+）：`mod.rs` + `windows.rs`（环境感知、敏感应用黑名单） |
| `intent.rs` | 意图引擎 RuleRouter（0.4+）：关键字→动作路由 |
| `clipboard.rs` | 剪贴板历史（0.7+）：监听/存储/搜索/管理 |
| `perf.rs` | 性能统计（0.7+）：SQLite 指标存储 + RAII Timer + 百分位查询 |
| `text.rs` | 文本工具：字符串截断防御等 |
| `locale/` | 系统语言检测：`mod.rs` + `windows.rs` |
| `calc.rs` | `evalexpr` 实时计算，整数转浮点避免截断 |
| `service.rs` | Service 骨架：`Service` trait + `AppContext` + 各服务生命周期统一编排 |
| `history.rs` | SQLite 历史记录（频率加权 + 衰减）+ `config` 表 KV 读写 |

### 前端 (`frontend/`)

- `index.html` / `style.css` — 主搜索窗口（弹性大小、键盘导航）
- `js/main.js` — 入口，模块初始化
- `js/search.js` / `js/results.js` / `js/keyboard.js` — 搜索/结果/键盘导航
- `js/actions.js` / `js/contextmenu.js` — 动作执行/右键菜单
- `js/api.js` / `js/tauri.js` — Tauri invoke 封装
- `js/lifecycle.js` / `js/window-size.js` — 生命周期/弹性窗口
- `js/dom.js` / `js/hints.js` / `js/statusbar.js` — DOM 工具/提示/状态栏
- `js/i18n.js` — 国际化（0.5.5+）
- `js/theme.js` / `theme.css` — 主题系统（0.5.4+）：CSS 变量（dark=Mocha/light=Latte）+ auto 跟随系统
- `settings.html` / `settings.js` / `settings.css` — 设置窗口（通用/快捷键/引擎/插件/网络/上下文/存储/调试/关于）
- `contextmenu-popup.html` — 右键菜单弹窗

前端用 `window.__TAURI__.core.invoke()` 调 Rust commands，用 `TAU.event.listen()` 监听后端事件
（`blink://shown`、`blink://hidden`、`blink://results`——async lane 慢引擎/插件增量，前端 `results.merge` 按 seq 协调）。

### 关键子系统实现要点

- **搜索缓存**（`search/start_menu_engine.rs`）：启动后台预扫开始菜单到内存，输入时命中缓存。所有文件 IO 在 `spawn_blocking`；根目录 mtime 增量失效 + 定时强制刷新兜底深层变化。0.7 增加增量更新 + 权重衰减。
- **图标协议**（`search/icon.rs` + `main.rs`）：自定义协议 `blink-icon`，前端 `<img src="http://blink-icon.localhost/<encodeURIComponent(lnk_path)>">`（**Windows 下 scheme 映射为 `http://<scheme>.localhost/`**，非 `scheme://localhost/`）。后端用 `IShellItemImageFactory::GetImage` 取图标 → `GetDIBits` 取 BGRA → 转 RGBA → `png` crate 编码 PNG。COM 每次调用 `CoInitializeEx`（RAII guard 配对）。Shell 名称解析 API 要求**规范全反斜杠路径**，调用前把 `/` 归一化为 `\`（不改扫描路径本身，见 §5）。进程内缓存 PNG 字节，`None` 缓存「提过但无图标」避免反复重试。
- **热键触发判定**（`hotkey/windows.rs`）：**不自维护按键累积镜像**（被系统合成事件打乱且无法自愈，曾导致 Alt+空格 残留误触发）。改为只在主键 down/up 边界用 `GetAsyncKeyState` 现查修饰键物理态；状态机仅 `down_since/armed_key/aborted` 三字段。修饰键精确 bitmask 匹配（不允许多余键，`Ctrl+Alt+空格` 不误触发 `Alt+空格`）。**改核心前读 `production-design/phases/0.3-plugin-skeleton.md` 二章**。
- **看门狗失焦**（`window/windows.rs`）：按**进程 PID** 判定前台是否属自己（非死比单个 HWND）；`fg==NULL`（焦点真空，子进程拉起等瞬态）跳过本轮不隐藏。
- **配置/自启/日志**：配置存 SQLite `config` 表（分命名空间：`app_config` / `engine:{id}` / `plugin:{id}` / `context:{key}`）；开机自启用 `tauri-plugin-autostart`（注册表 Run 项），启动时按配置同步。各模块内部 AtomicUsize/RwLock 实现热生效。

### Tauri Commands（前端 invoke 入口）

搜索/启动：`search_apps` / `launch_app` / `open_url` / `open_containing_folder` / `open_file_dialog` / `open_lnk_target` /
窗口：`hide_window` / `resize_window` / `hide_settings_window` /
配置：`get_config` / `update_general_config` / `update_hotkey` / `update_tap_threshold` / `update_grace_period` / `update_auto_start` / `update_language` / `update_log_level` / `reset_config` / `update_global_proxy` /
引擎配置：`get_engine_config` / `update_engine_config` / `get_calc_config` / `update_calc_config` / `get_start_menu_config` / `update_start_menu_config` / `update_file_search` / `probe_everything` /
插件：`get_plugins` / `update_plugin_config` / `probe_interpreters` / `update_interpreter_config` /
上下文：`get_context_config` / `update_context_config` / `list_running_processes` /
右键菜单：`show_context_menu` / `hide_context_menu` / `context_menu_action` /
剪贴板：`get_clipboard_history` / `search_clipboard_history` / `clear_clipboard_history` / `delete_clipboard_item` / `copy_to_clipboard` / `record_clipboard_hit` / `get_clipboard_stats` /
性能：`get_perf_overview` / `get_perf_percentiles` / `get_perf_slow_queries` / `get_perf_recent` / `export_perf_report` / `clear_perf_data` /
历史/存储：`record_hotkey` / `get_storage_info` / `clear_history` / `reset_item_history` /
日志：`open_log_file` / `open_log_dir` / `get_log_info`

**新增 command 流程**：① `commands.rs` 加 `#[tauri::command]` 函数 → ② `main.rs` `invoke_handler!` 注册 → ③ 前端 `invoke("name", { args })`。

### 数据存储

SQLite `%APPDATA%\blink\blink.db`：
- `history(lnk_path, hit_count, last_used_at)` — 启动历史，频率加权
- `config(key, value, updated_at)` — 配置 KV

## 7. 编码风格与约定

- **可选行为优先配置化**：默认值用户可能想改的，做成配置项 + 合理默认；纯内部参数不暴露。详见 `production-design/phases/0.2-core-plugin-design.md` §6。
- **统一日志分级规范**：一律用 `tracing`（带模块 target），禁止散落 `println!/eprintln!`。级别严格区分如下：
  - **error（1）**：真正异常，功能受影响 —— 数据库失败、关键操作失败、配置加载失败
  - **warn（2）**：潜在问题，不影响运行 —— 多次重试、配置异常但用默认值兼容
  - **info（3）**：关键状态变化 —— 应用启动、配置更新、插件加载、用户关键操作
  - **debug（4）**：主流程节点 —— 收到搜索请求、返回结果数、分支决策条件
  - **trace（5）**：诊断细节 —— HTTP 响应体、完整参数、边界条件触发、循环内变量
- **日志最佳实践**：
  - 结构化日志优先：`tracing::debug!(%query, port, "搜索 Everything")` 而非字符串拼接
  - 错误信息带上下文：`tracing::error!(%path, %e, "启动应用失败")` 而非仅 "失败了"
  - 预期内降级用 debug/trace：Everything 没装、剪贴板被锁定、图标提取失败 → 这些是设计好的降级路径，不是异常
  - 敏感信息永不记日志：密码、剪贴板内容（除 trace 级谨慎使用）、用户隐私数据
- **改完先自审**：每次完成改动后自己先 review（diff / 编译 / 副作用）再报告。
- **平台抽象预留**：平台相关逻辑走 `mod.rs` 接口 + `windows.rs` 实现的拆分，方便后续多平台。
- **自用阶段**：插件 permissions 权限模型暂不实现，产品化时再补。
- **0.x 阶段不发布，不做过度工程**：不到 1.0 版本不会对外发布，manifest 升级、向后兼容、权限强制、第三方插件目录等「产品化基础设施」在 1.0 前不做。优先验证功能链路，不提前造无用抽象。

## 8. TDD 开发与测试（务实 TDD）

bin crate，无 lib target，跑测试用 `cargo test --bin blink`。

- **纯逻辑/算法必须有单测，优先先写测试**：计算（calc）、fuzzy/拼音、PNG 编码、状态机等可纯函数化的逻辑。设计时主动把可测逻辑从平台调用里抽出来（如 `icon.rs` 的 `encode_rgba_to_png` 纯函数）。
- **Win32 / GUI / Shell / Tauri 集成层免自动化测试**：这类调用难以稳定 mock，靠 `cargo run` 手动验证主链路。
- **依赖系统资源的测试要可跳过**：如需真实系统文件，用 `Path::exists` 守卫，缺失则跳过（不依赖 CI 桌面环境）。
- **测试要验证产物正确性**，而非仅「非空」——例如断言 PNG 魔数而不只是 `!is_empty()`。

## 9. 其他相关内容

设计文档（`production-design/`，**改核心前先读 [production-design/README.md](docs/production-design/README.md) 了解目录导航与运作规则**）：
- `00-overview.md` — 产品/架构总纲（原 MVP.md；P0-P4、§12 待决策、§13 已确认方案）
- `product-interaction.md` / `product-platform.md` / `product-context-future.md` / `product-principles.md` — 产品设计四卷（原 product-design.md 按域拆分：交互/插件意图AI/Context隐私/取舍规范时间线）
- `phases/0.1-base.md` — 0.1 MVP 实现总结与后期待办
- `phases/0.2-core-plugin-design.md` — **0.2 核心+插件架构设计**（Service/SearchEngine/Plugin/Intent，改核心前必读）
- `phases/0.3-plugin-skeleton.md` — **0.3 插件骨架 + 热键物理态重构**（改核心前读）
- `phases/0.4-intent-router.md` — 0.4 意图路由 RuleRouter + Context 层
- `phases/0.5-config-search-extension.md` — 0.5 配置架构 + 文件搜索 + 右键菜单 + 主题
- `phases/0.6-plugin-packaging-scripting.md` — 0.6 插件打包 + Python/Node.js 脚本支持
- `phases/0.7-plugin-ecosystem-local-search.md` — 0.7 插件生态 + 本地搜索 + 性能统计
- `phases/0.8-ai-intent-router.md` — 0.8 意图路由优化 + AI 能力（规划中）
