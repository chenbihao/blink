# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Blink 是一个 Windows 全局快捷入口，不是启动器，是「Universal Action Layer（统一操作层）」。终极目标是感知用户上下文、主动推荐动作，让任何操作比原来的路径更快。

当前处于 **0.1 MVP** 阶段，专注验证基础交互的可靠性，而非功能数量。

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

## MVP Priority Levels

| 级别 | 验证目标 | 当前状态 |
|---|---|---|
| **P0** | 基础交互可靠性（热键、窗口、焦点、IME） | ✅ 已完成 |
| **P1** | 搜索能力（应用搜索、计算、历史权重） | ✅ 已完成 |
| **P2** | 插件系统（独立进程 + stdin/stdout JSON） | ⬜ 待开发 |
| **P3** | AI 能力（规则 → 本地模型 → 云模型逐级降级） | ⬜ 待开发 |
| **P4** | 语音（VAD + STT） | ⬜ 待开发 |

## Build & Run

```bash
# 环境要求：Rust 1.75+ (edition 2024)、MSVC Build Tools、WebView2 Runtime

# 开发运行（debug 模式，控制台有 eprintln 日志）
cargo run

# 打包
cargo install tauri-cli
cargo tauri build
```

没有测试套件，没有 linter 配置。

## Architecture

**Tauri 2 桌面应用**：Rust 后端 + 纯 HTML/CSS/JS 前端（无 bundler、无 npm）。

### Rust 后端 (`src/`)

| 文件 | 职责 |
|---|---|
| `main.rs` | Tauri 初始化、托盘菜单、启动热键监听和看门狗 |
| `commands.rs` | Tauri command 层 — 前端 `invoke()` 的入口，轻量编排，不含业务实现 |
| `hotkey.rs` | 全局右 Alt 热键，`WH_KEYBOARD_LL` 低级键盘钩子 + tap/hold 状态机 |
| `window_ctl.rs` | 窗口显隐控制 + 常驻看门狗轮询前台窗口实现失焦检测 |
| `search.rs` | 扫描开始菜单 `.lnk` 文件 + `nucleo` fuzzy 匹配 + `pinyin` 拼音首字母 |
| `calc.rs` | `evalexpr` 实时计算，预处理整数转浮点避免截断 |
| `history.rs` | SQLite (`sqlx`) 存储启动历史，频率加权影响搜索排序 |

### 前端 (`frontend/`)

纯静态文件，无构建步骤。`tauri.conf.json` 的 `frontendDist` 直接指向此目录。

- `index.html` / `main.js` / `style.css` — 主搜索窗口
- `settings.html` / `settings.js` / `settings.css` — 设置窗口（托盘菜单打开）

前端通过 `window.__TAURI__.core.invoke()` 调用 Rust commands，通过 `TAU.event.listen()` 监听后端事件（`blink://shown`、`blink://hidden`、`blink://debug`）。

### 关键设计决策

**热键不吞键**：`hotkey.rs` 的 hook 回调全程 `CallNextHookEx` 放行，右 Alt 仍可作系统修饰键（组合键）。tap/hold 判定靠按压时长（≤300ms = tap）+ 期间是否出现其他键。这是架构级约束——做得差的产品在 keydown 就 `return 1` 吞掉右 Alt，导致右 Alt 无法再作系统修饰键。

**看门狗失焦检测**：`window_ctl.rs` 不依赖 `WM_ACTIVATE(deactivate)`，而是每 150ms 轮询 `GetForegroundWindow()`。invoke 后有 500ms grace period 覆盖焦点抖动。状态机：Hidden → Showing → Visible。选看门狗是因为某些窗口（如 IDEA 终端子进程）不发失焦通知。

**搜索双路匹配**：`search.rs` 同时对原始名和拼音首字母做 nucleo fuzzy 匹配，取最高分。历史 hit_count 通过 `ln(hit+1)*100` 加权。

**窗口定位**：唤起时定位到前台应用所在显示器的正中央（物理像素计算，考虑 DPI 缩放）。

## Tauri Commands（前端 invoke 入口）

- `search_apps(query)` — 先尝试计算，失败则 fuzzy 搜索
- `launch_app(lnk_path)` — 打开 lnk 并记录历史
- `hide_window()` — 隐藏主窗口
- `resize_window(width, height)` — 弹性窗口调整
- `get_storage_info()` / `clear_history()` — 设置页存储管理

## Adding a New Tauri Command

1. 在 `src/commands.rs` 添加 `#[tauri::command]` 函数
2. 在 `src/main.rs` 的 `invoke_handler` 宏中注册
3. 前端通过 `invoke("command_name", { args })` 调用

## Data Storage

SQLite 数据库路径：`%APPDATA%\blink\blink.db`，单表 `history(lnk_path, hit_count, last_used_at)`。

## Roadmap Context

后续演进方向（了解即可，不要提前实现）：

- **搜索引擎拆分**：当前每次调用都重新扫描开始菜单，后续拆分为独立引擎（开始菜单/文件/意图识别），每个引擎独立缓存/增量更新
- **插件系统**：独立进程 + stdin/stdout JSON 协议，manifest 声明 triggers/permissions/capabilities
- **AI 路由**：规则优先 → 本地小模型（可选重型插件）→ 云模型逐级降级，不要默认把所有输入发给 AI
- **Context 层**：感知前台应用/选中文本/剪贴板等环境信息，用于意图识别和主动建议
- **配置持久化**：TOML 配置文件（`%APPDATA%\blink\config.toml`）+ SQLite 业务数据 + Windows Credential Manager 敏感凭证
