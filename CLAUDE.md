# CLAUDE.md

本文件为 Claude Code 在本仓库工作时提供指引。**指令优先级高于默认行为，必须严格遵循。**

更新时间 20260624

## 1. 项目概览

Blink 是一个 Windows 全局快捷入口，定位不是「启动器」，而是「Universal Action Layer（统一操作层）」。
终极目标：感知用户上下文、主动推荐动作，让任何操作都比原来的路径更快。

当前处于 **0.2** 阶段：0.1 MVP（基础交互 + 搜索 + 配置）已完成，0.2 进行核心服务化 + 多路搜索 + 插件系统的架构演进。

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
cargo tauri build        # 打包（需先 cargo install tauri-cli）
cargo test --bin blink   # 跑单测（bin crate，无 lib target）
```

## 4. Roadmap

| 版本 | 内容 | 状态 |
|---|---|---|
| **0.1** | P0 基础交互（热键/窗口/焦点/IME）+ P1 搜索（应用/计算/历史权重）+ 配置系统/设置页/热键录制 | ✅ |
| **0.2.0** | 0.1 收尾：搜索缓存、开机自启、tracing 日志、窗口定位、应用图标 | ✅ |
| **0.2.1** | Service 骨架（现有模块重构进 Service 框架，功能零变化） | ⬜ |
| **0.2.2** | SearchService + SearchEngine trait + 渐进式多路搜索（sync/async 双 lane） | ⬜ |
| **0.3+** | 插件系统（独立进程 + stdio JSON + manifest）/ 意图引擎（规则→向量→AI）/ Context 层 | ⬜ |

**改核心前必读 `production/0.2-core-plugin-design.md`**（Service/SearchEngine/Plugin/Intent 架构设计）。
Roadmap 内容了解即可，**不要提前实现**。

## 5. 业务相关决策

这些是无法从代码直接推断、影响实现取舍的关键决策：

- **热键不吞键**：hook 回调全程 `CallNextHookEx` 放行，右 Alt 仍可作系统修饰键。tap/hold 靠按压时长（≤tap_threshold）+ 期间是否出现其他键区分。**架构级约束**——做得差的产品在 keydown 就吞掉右 Alt。
- **看门狗失焦检测**：不依赖 `WM_ACTIVATE`，每 150ms 轮询 `GetForegroundWindow()`。因为某些窗口（如 IDEA 终端子进程）不发失焦通知。invoke 后有 grace period 覆盖焦点抖动。
- **搜索双路匹配**：同时对原始名和拼音首字母做 nucleo fuzzy 取最高分；历史 `ln(hit+1)*100` 加权。
- **图标懒加载**：图标提取**不进搜索热路径**，由自定义协议 `blink-icon` 按需提供（见 §6）。
- **lnk_path 是 history 主键**：扫描产生的路径字符串不可随意归一化/改写，否则历史权重 key 失配。

## 6. 高层架构与模块拆分

平台相关模块拆为 `mod.rs`（平台接口 + 通用逻辑）+ `windows.rs`（Windows 实现），为多平台预留。

### Rust 后端 (`src/`)

| 文件 | 职责 |
|---|---|
| `main.rs` | Tauri 初始化、托盘、tracing、自定义协议注册、启动各后台任务 |
| `commands.rs` | Tauri command 层 — 前端 `invoke()` 入口，轻量编排 |
| `config.rs` | `AppConfig`（快捷键/tap阈值/grace/自启/语言/日志级别）SQLite 持久化 + 运行时热更新 |
| `logging.rs` | tracing：文件每日轮转（保留 7 天）+ 控制台 + 动态级别 reload |
| `hotkey/` | 全局热键：`mod.rs`(tap/hold 状态机) + `windows.rs`(WH_KEYBOARD_LL) + `recorder.rs`(录制状态机) |
| `window/` | 窗口控制：显隐、看门狗失焦轮询、显示器定位（读实际物理尺寸定位前台显示器正中） |
| `search/` | `mod.rs`(AppEntry/fuzzy/拼音) + `windows.rs`(扫描开始菜单 .lnk) + `cache.rs`(结果缓存) + `icon.rs`(图标提取) |
| `calc.rs` | `evalexpr` 实时计算，整数转浮点避免截断 |
| `history.rs` | SQLite 历史记录（频率加权）+ `config` 表 KV 读写 |

### 前端 (`frontend/`)

- `index.html` / `main.js` / `style.css` — 主搜索窗口（弹性大小、键盘导航）
- `settings.html` / `settings.js` / `settings.css` — 设置窗口（5 Tab：通用/快捷键/存储/调试/关于）

前端用 `window.__TAURI__.core.invoke()` 调 Rust commands，用 `TAU.event.listen()` 监听后端事件
（`blink://shown`、`blink://hidden`；`blink://results` 为渐进式增量预留）。

### 关键子系统实现要点

- **搜索缓存**（`search/cache.rs`）：启动后台预扫开始菜单到内存，输入时命中缓存。所有文件 IO 在 `spawn_blocking`；根目录 mtime 增量失效 + 定时强制刷新兜底深层变化。
- **图标协议**（`search/icon.rs` + `main.rs`）：自定义协议 `blink-icon`，前端 `<img src="http://blink-icon.localhost/<encodeURIComponent(lnk_path)>">`（**Windows 下 scheme 映射为 `http://<scheme>.localhost/`**，非 `scheme://localhost/`）。后端用 `IShellItemImageFactory::GetImage` 取图标 → `GetDIBits` 取 BGRA → 转 RGBA → `png` crate 编码 PNG。COM 每次调用 `CoInitializeEx`（RAII guard 配对）。Shell 名称解析 API 要求**规范全反斜杠路径**，调用前把 `/` 归一化为 `\`（不改扫描路径本身，见 §5）。进程内缓存 PNG 字节，`None` 缓存「提过但无图标」避免反复重试。
- **配置/自启/日志**：配置存 SQLite `config` 表（`app_config` 键存 JSON）；开机自启用 `tauri-plugin-autostart`（注册表 Run 项），启动时按配置同步。

### Tauri Commands（前端 invoke 入口）

`search_apps` / `launch_app` / `hide_window` / `resize_window` /
`get_config`、`update_hotkey`、`update_tap_threshold`、`update_grace_period`、`update_auto_start`、`update_language`、`update_log_level`、`reset_config` /
`record_hotkey` / `get_storage_info`、`clear_history` / `open_log_file`、`open_log_dir`、`get_log_info`

**新增 command 流程**：① `commands.rs` 加 `#[tauri::command]` 函数 → ② `main.rs` `invoke_handler!` 注册 → ③ 前端 `invoke("name", { args })`。

### 数据存储

SQLite `%APPDATA%\blink\blink.db`：
- `history(lnk_path, hit_count, last_used_at)` — 启动历史，频率加权
- `config(key, value, updated_at)` — 配置 KV

## 7. 编码风格与约定

- **可选行为优先配置化**：默认值用户可能想改的，做成配置项 + 合理默认；纯内部参数不暴露。详见 `production/0.2-core-plugin-design.md` §6。
- **统一日志**：一律用 `tracing`（带模块 target），禁止散落 `println!/eprintln!`。正常路径用 `debug!/trace!`，真正异常才 `warn!/error!`——像图标提取这类「单项失败属正常降级」的场景，失败要静默（debug 级），不可刷屏。
- **改完先自审**：每次完成改动后自己先 review（diff / 编译 / 副作用）再报告。
- **平台抽象预留**：平台相关逻辑走 `mod.rs` 接口 + `windows.rs` 实现的拆分，方便后续多平台。
- **自用阶段**：插件 permissions 权限模型暂不实现，产品化时再补。

## 8. TDD 开发与测试（务实 TDD）

bin crate，无 lib target，跑测试用 `cargo test --bin blink`。

- **纯逻辑/算法必须有单测，优先先写测试**：计算（calc）、fuzzy/拼音、PNG 编码、状态机等可纯函数化的逻辑。设计时主动把可测逻辑从平台调用里抽出来（如 `icon.rs` 的 `encode_rgba_to_png` 纯函数）。
- **Win32 / GUI / Shell / Tauri 集成层免自动化测试**：这类调用难以稳定 mock，靠 `cargo run` 手动验证主链路。
- **依赖系统资源的测试要可跳过**：如需真实系统文件，用 `Path::exists` 守卫，缺失则跳过（不依赖 CI 桌面环境）。
- **测试要验证产物正确性**，而非仅「非空」——例如断言 PNG 魔数而不只是 `!is_empty()`。

## 9. 其他相关内容

设计文档（`production/`）：
- `Windows Universal Launcher MVP.md` — 产品/架构总纲（P0-P4、§12 待决策、§13 已确认方案）
- `0.2-core-plugin-design.md` — **0.2 核心+插件架构设计**（改核心前必读）
- `Todo-0.1mvp.md` — 0.1 实现总结与后期待办
