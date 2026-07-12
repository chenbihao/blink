<h1 align="center">Blink</h1>

<p align="center">
  <strong>Windows 全局操作层 —— 感知你在做什么，让每一个操作都比原来更快。</strong>
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

[查看演示动图](docs/images/blink.gif)

---

## 这是什么

Blink 不是启动器（launcher），是 **Universal Action Layer（统一操作层）**：
搜索只是入口之一，**动作执行才是终点**。感知上下文、主动推荐动作，让"选中一段英文 → 翻译"、"复制了 URL → 打开链接"、"截图 → 存剪贴板"这类操作从**多步骤**变成**零输入或一次 Tab**。

- ⚡ **极速** — 唤起 &lt; 50ms，首个结果 &lt; 20ms
- 🎯 **懂上下文** — 选中/剪贴板/前台应用自动感知，不打扰但随手可用
- 🧩 **可扩展** — 独立进程插件（Rust/Python/Node.js/PowerShell），崩溃不影响核心

---

## 核心能力

### 极速搜索

- **应用搜索** — 扫描开始菜单，模糊匹配 + 拼音首字母 + 全拼（`wx` → 微信、`dksz` → 打开设置）
- **文件搜索** — 集成 Everything，未安装时回退到本地目录扫描
- **实时计算** — `1+1` / `100*0.25` 回车即复制
- **剪贴板历史** — 后台自动记录，支持模糊搜索

### 上下文感知

- **划词感知** — 在其他应用选中文本，Blink 自动抓取，唤起后一键翻译/搜索
- **Ghost Text 补全** — 输入 `fy hello`，自动提示 `fanyi hello`，按 Tab 触发翻译
- **智能推荐** — 剪贴板是 URL/文件路径 → 自动推荐"打开链接/在资源管理器定位"；选中英文 → Ghost 建议翻译
- **弱意图不打扰** — 感知在后台静默运行，看到就用、看不到不碍事；每项功能都可在设置里单独关闭

### Chord 快捷动作

主窗打开时按住 Alt，字母键直达动作：

| 组合 | 动作 |
|---|---|
| `Alt + 1~9` | 快速触发结果列表中对应位置的项 |
| `Alt + C` | 剪贴板历史 |
| `Alt + A` | 区域截图（框选区域 → 剪贴板 / 保存） |

### 插件生态

- **独立进程** — JSONL 通信，崩溃不影响核心；支持 Rust / Python / Node.js / PowerShell
- **内置插件** — 翻译（有道 / 百度 / DeepL / 阿里 / 腾讯）、天气、IP 查询
- **展示方式灵活** — 插件可混排在结果中 / 置顶显示 / 独占整个返回区

### 体验细节

- **右键菜单** — 按结果类型智能推荐（打开位置、复制路径、以管理员运行等）
- **主题** — Catppuccin Mocha 暗色 / Latte 亮色 / 跟随系统
- **国际化** — 中文 / English 全项目双语（含插件文案）
- **可配置** — 快捷键、引擎开关、开机自启、代理、日志级别、上下文感知逐条开关

---

## 路线图

**已完成：** 基础搜索 → 插件生态 → 上下文感知 → Chord 交互 → AI 对话能力 → 前端架构重整

**下一步：**

| 方向 | 内容 | 状态 |
|---|---|---|
| 🎤 **语音指令** | STT · 双 chord 语音入口 · 语音找文件 · Agent 对话窗口 | 📋 规划中 |
| 🔌 **本地化与生态** | 本地模型按需下载 · skill 化 · MCP 双向 · RAG 记忆 | 📋 规划中 |
| 🛡️ **信任边界** | AI 只推荐动作，不直接执行；确认权始终在你手中 | ✅ 已内建 |

---

## 安装

从 [Releases](../../releases) 下载最新安装包。

---

## 使用

1. 启动后 Blink 常驻系统托盘
2. **`Alt + Space`** → 唤起输入框
3. 输入应用名（拼音首字母/全拼皆可）、算式、插件触发词
4. **↑↓** 选择，**Enter** 或 **Alt + 1~9** 快速触发
5. 主窗打开时按住 **Alt** → 可看到快捷动作提示
6. **Esc** 或失焦 → 隐藏

### 快捷键速查

| 操作 | 说明 |
|---|---|
| `Alt + Space` | 唤起 / 隐藏 |
| `↑` `↓` | 上下选择 |
| `Alt + 1~9` | 快速触发结果列表中对应位置的项 |
| `Tab` | 采纳补全建议（如 `fy` → `fanyi `）或上下文推荐 |
| `Enter` | 启动 / 复制结果 |
| `PgUp` `PgDn` | 翻页 |
| `Alt + C` | Chord：剪贴板历史 |
| `Alt + A` | Chord：区域截图 |
| `Esc` | 隐藏 |

---

## 开发

### 环境要求

- [Rust](https://www.rust-lang.org/tools/install) 1.75+
- [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)（MSVC C++ 工作负载）
- [WebView2 Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/)（Windows 10/11 通常已预装）

### 构建运行

```bash
# 开发模式（debug，控制台日志）
cargo tauri dev

# 打包发布（含插件编译）
cargo xtask release

# 运行测试
cargo test --bin blink
```

### 想改核心？

先读 [`docs/production-design/`](docs/production-design/README.md)：
- [`00-overview.md`](docs/production-design/00-overview.md) — 产品愿景 + 里程碑
- [`product-platform.md §5.0`](docs/production-design/product-platform.md) — 四域架构铁则
- [`phases/{version}-*.md`](docs/production-design/phases/) — 各版本设计决策 + 踩坑记录

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
