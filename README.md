<h1 align="center">Blink</h1>

<p align="center">
  <strong>Windows 丝滑启动器 —— 感知你在做什么，让每一个操作都比原来更快。</strong>
</p>

<p align="center">
  <a href="README_EN.md">English</a> · <a href="README.md">中文</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/platform-Windows%2010%2F11-blue?style=flat-square" alt="Platform"/>
  <img src="https://img.shields.io/badge/rust-2024-orange?style=flat-square&logo=rust" alt="Rust"/>
  <img src="https://img.shields.io/badge/tauri-2-blue?style=flat-square&logo=tauri" alt="Tauri 2"/>
  <img src="https://img.shields.io/badge/license-MIT-green?style=flat-square" alt="License"/>
</p>

<p align="center">
  <img src="docs/images/blink-rust.png" width="680" alt="Blink Demo"/>
</p>

---

## 不只是启动器

**做一个极其丝滑的启动器，并且把常用的功能都丝滑融合，使用 Chord 模式来调用各种增强能力，不止是启动器。**

Blink 把"选中英文 → 翻译"、"复制了 URL → 打开"、"截图 → OCR 提取文字"这类多步骤操作，变成一次快捷键或一个 Tab。整个体验围绕**丝滑**展开——唤起快、响应快、操作路径短。

通过 **Chord 模式**（主窗口按住 Alt + 字母键）可以快速调用截图、翻译、语音输入等增强能力，不用切窗口、不用记复杂快捷键。

底层是**能力化架构**：每个功能（搜索、截图、翻译、插件……）都封装为标准 Capability，可供 AI 调用。未来这些能力也会开放给外部 AI 工具，作为 CLI 工具或 Skill 调用。

---

## 极速入口

<p align="center">
  <img src="docs/images/feature-search.gif" width="680" alt="极速搜索"/>
</p>

- **应用搜索** — 模糊匹配 + 拼音首字母 + 全拼（`wx` → 微信、`dksz` → 打开设置）
- **文件搜索** — 集成 Everything，未安装时回退到本地目录扫描
- **实时计算** — `sqrt(144)` / `0.75*360` 回车即复制
- **剪贴板历史** — 后台记录，支持模糊搜索
- **Tab 快速采纳** — 输入 `fy` 按 Tab 自动补全为 `fanyi `，直接开始翻译
- **极速响应** — 唤起 < 50ms，首个结果 < 20ms

---

## 智能感知

<p align="center">
  <img src="docs/images/feature-context.gif" width="680" alt="上下文感知"/>
</p>

Blink 会在后台静默感知你的上下文，但不会打扰你——看到就用，看不到不碍事。

- **划词感知** — 在其他应用选中文本，唤起 Blink 后自动抓取，一键翻译或搜索（有些应用未实现 UIA 文本接口，划词可能无法感知，可手动 `Ctrl+C` 复制后唤起，Blink 会自动读取剪贴板内容）
- **剪贴板感知** — 复制了 URL？自动推荐打开链接。复制了英文？自动推荐通过插件翻译
- **Ghost Text 补全** — 输入 `fy`，灰色提示 `fanyi `，按 Tab 快速采纳，直接开始输入要翻译的内容
- **逐项可控** — 每种感知都可以在设置里单独关闭

---

## Chord 增强

<p align="center">
  <img src="docs/images/feature-chord.gif" width="680" alt="Chord 模式"/>
</p>

主窗口打开时按住 Alt，字母键变成快捷动作入口。不用记全局快捷键，用的时候看一眼提示就行。

| 组合 | 作用                           |
|---|------------------------------|
| `Alt + Q` | 唤起 AI 窗口进行对话（后续版本实现）         |
| `Alt + A` | 区域截图 → OCR / 翻译 / 钉图 / 保存    |
| `Alt + C` | 剪贴板历史                        |
| `Alt + Space` | 语音输入（支持在主窗口输入，也支持作为单独的语音输入法） |
| `Alt + 1~9` | 快速触发结果列表中对应位置的项              |

截图后支持标注（矩形、箭头、文字、马赛克等）、OCR 文字识别、图上翻译、钉图置顶。按住 `Alt + Space` 可以语音输入，支持云端（OpenAI / Groq）和本地（FunASR）双引擎，边说边出字，VAD 自动切句。

---

## 插件生态

<p align="center">
  <img src="docs/images/feature-plugin.gif" width="680" alt="插件生态"/>
</p>

插件运行在独立进程，崩溃不影响 Blink 核心。支持 Python、Node.js、Rust 或任何可执行文件。

- **翻译** — 有道 / 百度 / DeepL / 阿里 / 腾讯，五引擎内置
- **天气查询** — 简洁的天气卡片
- **IP 查询** — 快速查看当前网络信息
- **更多可能** — 用你熟悉的语言写插件，JSONL 协议通信，几行代码就能接入
- **未来** — 一键探索系统中的 CLI 工具，自动配置为 Blink 插件

---

## AI 就绪

<p align="center">
  <img src="docs/images/feature-ai.gif" width="680" alt="AI 能力"/>
</p>

<p align="center">
  <img src="docs/images/feature-ai-config.png" width="680" alt="AI 能力"/>
</p>


Blink 的每个功能都封装为标准 Capability（能力），可以被 AI 调用：

- **Action 调用** — AI 可以触发 Blink 的内置动作（搜索、打开应用、执行命令……）
- **插件调用** — AI 可以调用任何已安装的插件
- **多供应商支持** — 内置 OpenAI、DeepSeek、Groq 等预设，可配置任何兼容协议的供应商或本地大模型（ollama / lmstudio）
- **未来开放** — 这些能力会逐步开放给外部 AI 工具，作为 CLI 工具或 Skill 调用

目前 AI 对话窗口正在开发中（`Alt + Q`），未来可以用自然语言指挥 Blink 完成复杂操作。

---

## 路线图

**已完成：** 基础搜索 → 插件生态 → 上下文感知 → Chord 交互 → 语音输入 → 截图标注 → AI 基础调用链路

**下一步：**

- **对话窗口** — 独立 Agent 窗口，自然语言指挥
- **MCP 双向** — 让 Blink 的能力被外部 AI 工具调用
- **记忆功能** — 让 AI 记住你的偏好和历史，越用越懂你

---

## 安装

从 [Releases](../../releases) 下载最新安装包。

---

## 使用

1. 启动后 Blink 常驻系统托盘
2. `Alt + Space` 唤起输入框
3. 输入应用名（拼音首字母/全拼皆可）、算式、或插件触发词
4. `↑` `↓` 选择，`Enter` 或 `Alt + 1~9` 快速触发
5. 主窗口打开时按住 `Alt` 可以看到 Chord 快捷动作
6. `Esc` 或失焦隐藏

### 快捷键速查

| 操作 | 说明 |
|---|---|
| `Alt + Space` | 唤起 / 隐藏 |
| `↑` `↓` | 上下选择 |
| `Tab` | 采纳补全建议（如 `fy` → `fanyi `）或上下文推荐 |
| `Enter` | 启动 / 复制结果 |
| `Alt + Q` | AI 对话窗口（后续版本） |
| `Alt + A` | 区域截图 |
| `Alt + C` | 剪贴板历史 |
| `Alt + 1~9` | 快速触发对应位置的结果 |
| `Alt + Space`（按住） | 语音输入 |
| `Esc` | 隐藏 |

---

## 开发

### 环境要求

- [Rust](https://www.rust-lang.org/tools/install) 1.75+
- [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)（MSVC C++ 工作负载）
- [WebView2 Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/)（Windows 10/11 通常已预装）

### 构建运行

```bash
# 开发模式
cargo tauri dev

# 打包发布（含插件编译）
cargo xtask release

# 运行测试
cargo test --bin blink
```

### 架构概览

```
┌─────────────────────────────────────────────────────┐
│  Frontend (WebView2)                                │
│  纯 HTML/CSS/JS，无构建步骤                          │
└──────────────────────┬──────────────────────────────┘
                       │ invoke / event
┌──────────────────────┴──────────────────────────────┐
│  Tauri Commands Layer                               │
│  IPC 入口，连接前端与后端                              │
└──────────────────────┬──────────────────────────────┘
                       │
┌──────────────────────┴──────────────────────────────┐
│  Domain Layer (四域架构)                             │
│                                                     │
│  Awareness    ── 感知上下文（剪贴板/划词/前台应用）     │
│  Suggestion   ── 生产建议（Ghost Text / 智能推荐）     │
│  Routing      ── 路由决策（Query → 最佳动作）          │
│  Execution    ── 执行动作（统一入口，显式触发）         │
│                                                     │
│  SearchEngine ── 搜索引擎（应用/文件/计算器/剪贴板）   │
│  Capability   ── 能力层（截图/OCR/翻译/插件……）       │
│  AI Provider  ── AI 接口（rig-core，可切换供应商）     │
└──────────────────────┬──────────────────────────────┘
                       │
┌──────────────────────┴──────────────────────────────┐
│  Infrastructure Layer                               │
│  SQLite / Win32 API / 平台抽象 / 插件进程管理         │
└─────────────────────────────────────────────────────┘
```

### 想深入？

- [`docs/production-design/00-overview.md`](docs/production-design/00-overview.md) — 产品愿景与里程碑
- [`docs/production-design/phases/`](docs/production-design/phases/) — 各版本设计决策与踩坑记录

---

## 致谢

- [Wox](https://github.com/Wox-launcher/Wox) — Windows 上的先驱 Launcher
- [Alfred](https://www.alfredapp.com/) — 证明了全局输入框可以成为人机交互的第一入口
- [Raycast](https://www.raycast.com/) — 现代化 Launcher 体验，插件生态的标杆
- [uTools](https://u.tools/) — 国产效率工具，本地化体验的参考
- [Flow Launcher](https://www.flowlauncher.com/) — Windows 上的开源 Launcher，社区驱动的典范
- [Everything](https://www.voidtools.com/) — 极速文件搜索，Blink 文件搜索的集成方案
- [Quicker](https://getquicker.net/) — Chord 交互模式的灵感来源
 
---

## 许可

[MIT](LICENSE)
