<h1 align="center">Blink</h1>

<p align="center">
  <strong>Windows 全局操作层 —— 感知你在做什么，让任何操作都比原路径更快。</strong>
</p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.zh-CN.md">中文</a>
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

## 功能

### 基础搜索

- **极速唤起** — `Alt + Space`（可自定义右 Alt tap），失焦自动隐藏
- **应用搜索** — 扫描开始菜单，模糊匹配 + 拼音首字母 + 全拼（`wx` → 微信、`dksz` → 打开设置）
- **文件搜索** — 集成 Everything，未安装时降级为本地目录扫描
- **实时计算** — `1+1` / `100*0.25` 回车复制
- **剪贴板历史** — 后台自动记录，支持 fuzzy 搜索历史

### 感知与主动建议（0.8）

- **划词感知** — 在其他应用选中文本，Blink 自动抓取（UIA + 鼠标钩子），唤起后一键翻译/搜索
- **Ghost Text 补全** — 输入 `fy hello` 灰色行内提示 `→ fanyi hello`，Tab 采纳升级为翻译
- **Context 智能推荐** — 剪贴板是 URL/文件路径 → 自动推荐"打开链接/在资源管理器定位"；选中英文 → Ghost 建议翻译
- **弱意图 pull 不 push** — 感知在旁路，采纳零打扰，不采纳零代价；每条 Context 触发都可在设置逐条关闭

### Chord 交互（0.8.5，Alt hold 直达）

主窗打开时按住 Alt，字母键直达动作：

| 组合 | 动作 |
|---|---|
| `Alt + Q` | 划词翻译（独立悬浮球，去原应用选文本后 confirm，主窗还原 + 自动填充） |
| `Alt + C` | 剪贴板历史 top-9（Alt+1~9 快速选中回填） |
| `Alt + A` | 区域截图（0.8.7 规划中） |

### 插件与生态

- **插件系统** — 独立进程 + JSONL 通信，崩溃不影响核心；支持 Rust / Python / Node.js / PowerShell
- **内置插件** — 翻译（有道 / 百度 / DeepL / 阿里 / 腾讯）、天气、IP 查询
- **呈现权模型** — 触发与呈现正交，插件可选 `inline` 混排 / `priority` 置顶 / `takeover` 独占返回区

### 平台与体验

- **右键菜单** — 按结果类型智能推荐（打开位置、复制路径、以管理员运行、重置该项排序等）
- **主题** — Catppuccin Mocha 暗色 / Latte 亮色 / 跟随系统
- **国际化** — 中文 / English 全项目双语（含插件文案）
- **可配置** — 快捷键、引擎开关、开机自启、代理、日志级别、上下文感知逐条开关

---

## 愿景 · 我们要去哪

- 🤖 **AI 意图（0.9）** — 用自然语言描述你想做什么，Blink 自动匹配动作；AI 只产建议不直接执行，Tab 采纳是最后一道人类审核
- 💬 **AI Chat View（0.9）** — 支持 OpenAI / DeepSeek / 通义 / 豆包等 OpenAI 兼容 API，Chat 走 `view: chat` 展开
- 🧠 **VectorRouter 语义匹配（0.9）** — 意图分类不局限于 keyword，语义近似也能命中
- ⚡ **Proactive 主动建议** — 零输入，在对的时间给出对的动作
- 🎤 **语音输入** — VAD + 语音识别，用说的代替打字

---

## 安装

从 [Releases](../../releases) 下载最新安装包。

---

## 使用

1. 启动后 Blink 常驻系统托盘
2. **`Alt + Space`** → 唤起输入框
3. 输入应用名（拼音首字母/全拼皆可）、算式、插件触发词
4. **↑↓** 选择，**Enter** 或 **Alt + 1~9** 快速触发
5. 主窗打开时按住 **Alt** → Ghost 提示条显示 Chord 快捷键（Alt+A/Q/C）
6. **Esc** 或失焦 → 隐藏

### 快捷键速查

| 操作            | 说明 |
|---------------|---|
| `Alt + Space` | 唤起 / 隐藏 |
| `↑` `↓`       | 上下选择 |
| `Alt + 1~9`   | 快速触发对应位置 |
| `Tab`         | 采纳 Ghost 补全（`fy` → `fanyi `）或采纳 Context 建议 |
| `Enter`       | 启动 / 复制结果 |
| `PgUp` `PgDn` | 翻页 |
| `Alt + Q`     | Chord：划词翻译 |
| `Alt + C`     | Chord：剪贴板历史 |
| `Esc`         | 隐藏 |

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

## 路线图

| 版本 | 内容 | 状态 |
|---|---|---|
| **0.1 ~ 0.7** | 基础交互 → 插件生态 → 剪贴板历史 → 性能统计 | ✅ 完成 |
| **0.8.0 ~ 0.8.5** | UIA 划词 / 内置动作参数化 / Ghost Text / 翻译 Context / **四域架构** / Chord 交互底层 | ✅ 完成 |
| **0.8.6** | 架构固化（为 0.9 铺物理骨架：Action trait / SuggestionProducer / ConfigStore） | 🚧 进行中 |
| **0.8.7** | Alt+A 区域截图 | 📋 规划 |
| **0.9** | AI Provider 抽象 · 云端 AI 插件 · AI Chat View · VectorRouter 语义匹配 | 📋 规划 |
| **更远** | Proactive 主动建议 · 语音输入 · 插件市场 | 🔮 |

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
