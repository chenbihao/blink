# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Blink 是一个 Windows 全局快捷入口，不是启动器，是「Universal Action Layer（统一操作层）」。终极目标是感知用户上下文、主动推荐动作，让任何操作比原来的路径更快。

当前处于 **0.2** 阶段：0.1 MVP（基础交互 + 搜索 + 配置）已完成，0.2 进行核心服务化 + 多路搜索 + 插件系统的架构演进。**架构设计文档见 `production/0.2-core-plugin-design.md`（改核心前必读）。**

## Core Principle（最重要）

> **如果用户按快捷键后不能立即输入，其他所有功能都没有意义。**

所有改动都应服务于这条主链路的可靠性：`右 Alt 单击 → 窗口出现 → 自动 Focus → 用户直接输入 → ESC/失焦隐藏`。

## Performance Targets

| 指标 | 目标 |
|---|---|
| 快捷键唤起延迟 | < 50ms |
| 输入首个结果延迟 | < 20ms |
| 常驻内存 | < 300MB（Tauri + WebView2 基线约 80-150MB） |
| 输入焦点成功率 | > 99.9% |

## Version Status

| 版本 | 内容 | 状态 |
|---|---|---|
| **0.1** | P0 基础交互（热键/窗口/焦点/IME）+ P1 搜索（应用/计算/历史权重）+ 配置系统/设置页/热键录制 | ✅ |
| **0.2.0** | 0.1 收尾：搜索缓存、开机自启、tracing 日志、窗口定位读实际尺寸、文档同步 | ✅ |
| **0.2.1** | Service 骨架（现有模块重构进 Service 框架，功能零变化） | ⬜ |
| **0.2.2** | SearchService + SearchEngine trait + 渐进式多路搜索（sync/async 双 lane） | ⬜ |
| **0.3+** | 插件系统 / 意图引擎（见设计文档 §8） | ⬜ |

## Build & Run

```bash
# 环境要求：Rust（edition 2024）、MSVC Build Tools、WebView2 Runtime

# 开发运行（debug 模式，控制台 tracing 日志，默认 DEBUG 级）
cargo run

# 打包
cargo install tauri-cli
cargo tauri build
```

没有测试套件，没有 linter 配置。

## Architecture

**Tauri 2 桌面应用**：Rust 后端 + 纯 HTML/CSS/JS 前端（无 bundler、无 npm）。

### Rust 后端 (`src/`)

平台相关模块已拆分为 `mod.rs`（平台接口 + 通用逻辑）+ `windows.rs`（Windows 实现），为多平台预留（trait 抽象见各模块 TODO「方案 B」）。

| 文件 | 职责 |
|---|---|
| `main.rs` | Tauri 初始化、托盘菜单、tracing subscriber、启动热键/看门狗/搜索缓存/自启同步 |
| `commands.rs` | Tauri command 层 — 前端 `invoke()` 入口，轻量编排 |
| `config.rs` | 配置管理：`AppConfig`（快捷键/tap阈值/grace/自启/语言）SQLite 持久化 |
| `hotkey/` | 全局热键：`mod.rs`(tap/hold 状态机) + `windows.rs`(WH_KEYBOARD_LL) + `recorder.rs`(快捷键录制状态机) |
| `window/` | 窗口控制：`mod.rs`(接口) + `windows.rs`(显隐/看门狗失焦/显示器定位) |
| `search/` | 应用搜索：`mod.rs`(AppEntry/fuzzy/拼音首字母) + `windows.rs`(扫描开始菜单 .lnk) + `cache.rs`(结果缓存) |
| `calc.rs` | `evalexpr` 实时计算，整数转浮点避免截断 |
| `history.rs` | SQLite 历史记录（频率加权）+ `config` 表 KV 读写 |

### 前端 (`frontend/`)

纯静态文件，无构建步骤。`tauri.conf.json` 的 `frontendDist` 直接指向此目录。

- `index.html` / `main.js` / `style.css` — 主搜索窗口（弹性大小、键盘导航）
- `settings.html` / `settings.js` / `settings.css` — 设置窗口（5 Tab：通用/快捷键/存储/调试/关于）

前端通过 `window.__TAURI__.core.invoke()` 调用 Rust commands，通过 `TAU.event.listen()` 监听后端事件（`blink://shown`、`blink://hidden`；`blink://results` 为渐进式增量预留）。

### 关键设计决策

**热键不吞键**：`hotkey/` 的 hook 回调全程 `CallNextHookEx` 放行，右 Alt 仍可作系统修饰键。tap/hold 靠按压时长（≤tap_threshold）+ 期间是否出现其他键。架构级约束——做得差的产品在 keydown 就吞掉右 Alt。快捷键可配置（设置页录制）。

**看门狗失焦检测**：`window/` 不依赖 `WM_ACTIVATE(deactivate)`，每 150ms 轮询 `GetForegroundWindow()`。invoke 后有 grace period 覆盖焦点抖动。选看门狗是因为某些窗口（如 IDEA 终端子进程）不发失焦通知。

**搜索双路匹配**：`search/mod.rs` 同时对原始名和拼音首字母做 nucleo fuzzy，取最高分；历史 `ln(hit+1)*100` 加权。

**搜索缓存**：`search/cache.rs` 启动后台预扫开始菜单到内存，输入时命中缓存（不再每次重扫）；所有文件 IO 在 `spawn_blocking`；mtime 增量失效 + 定时强制刷新兜底深层变化。

**配置系统**：`config.rs` 的 `AppConfig` 存 SQLite `config` 表；设置页改快捷键（含录制）/tap阈值/grace/自启/语言，运行时热更新。

**开机自启**：`tauri-plugin-autostart`（注册表 Run 项），`update_auto_start` 写注册表 + 启动时按配置同步确保一致。

**统一日志**：`tracing` 替代散落的 eprintln；debug 构建 DEBUG / release INFO；带模块 target。

**窗口定位**：唤起时读窗口实际物理尺寸（`outer_size`），定位到前台应用所在显示器正中央。

## Tauri Commands（前端 invoke 入口）

- `search_apps(query)` — 先计算，失败则 fuzzy 搜索（命中缓存）
- `launch_app(lnk_path)` — 打开 lnk 并记录历史
- `hide_window()` — 隐藏主窗口
- `resize_window(width, height)` — 弹性窗口调整
- `get_config()` / `update_hotkey` / `update_tap_threshold` / `update_grace_period` / `update_auto_start` / `update_language` / `reset_config` — 配置读写
- `record_hotkey()` — 阻塞录制快捷键（spawn_blocking）
- `get_storage_info()` / `clear_history()` — 设置页存储管理

## Adding a New Tauri Command

1. 在 `src/commands.rs` 添加 `#[tauri::command]` 函数
2. 在 `src/main.rs` 的 `invoke_handler` 宏中注册
3. 前端通过 `invoke("command_name", { args })` 调用

## Data Storage

SQLite `%APPDATA%\blink\blink.db`：
- `history(lnk_path, hit_count, last_used_at)` — 启动历史，频率加权
- `config(key, value, updated_at)` — 配置 KV（`app_config` 键存 AppConfig JSON）

## Design Docs

- `production/Windows Universal Launcher MVP.md` — 产品/架构总纲（P0-P4、§12 待决策项、§13 已确认方案）
- `production/0.2-core-plugin-design.md` — **0.2 核心+插件架构设计**（Service/SearchEngine/Plugin/Intent，渐进式搜索，stdio 协议，子版本分期）
- `production/Todo-0.1mvp.md` — 0.1 实现总结与后期待办

## Roadmap Context（了解即可，不要提前实现）

- **搜索引擎拆分**：✅ 缓存已完成（0.2.0）；引擎 trait + 多路融合排序见设计文档 §2（0.2.2）
- **渐进式搜索**：sync/async 双 lane 首结果优先，见设计文档 §2.3（0.2.2）
- **插件系统**：独立进程 + stdin/stdout JSON + manifest，见设计文档 §3（0.3+）
- **意图引擎**：规则（keyword 独占路由）→ 向量 → AI，见设计文档 §4（0.3+）
- **Context 层**：感知前台应用/选中文本/剪贴板，见 MVP.md §13.7
- **配置持久化**：当前 SQLite config 表；后续可迁 TOML（MVP.md §13.5）

## Work Conventions（用户约定）

- **可选行为优先配置化**：默认值用户可能想改的，做成配置项 + 给合理默认；纯内部参数不暴露。详见 `production/0.2-core-plugin-design.md` §6。
- **自用阶段**：权限模型暂不实现（插件 permissions 不解析），产品化时再补。
