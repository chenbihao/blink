<h1 align="center">Blink</h1>

<p align="center">
  <strong>Windows 全局快捷入口，更快、更丝滑、更智能。</strong>
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
  <img src="docs/blink-rust.png" width="680" alt="Blink Demo"/>
</p>

[查看演示动图](docs/blink.gif)

---

## 功能特性

- **极速唤起** — `Alt + Space` 一键唤起，< 50ms 响应，失焦自动隐藏
- **应用搜索** — 扫描开始菜单，模糊匹配 + 拼音首字母（`wx` → 微信）
- **文件搜索** — 支持 Everything，未安装时自动降级为本地目录搜索
- **实时计算** — 输入 `1+1` 或 `100*0.25`，回车复制结果
- **剪贴板历史** — 搜索过往剪贴板内容
- **插件系统** — 原生插件（Rust）+ 脚本插件（Python / Node.js / PowerShell）
- **内置插件** — 翻译（有道/百度/DeepL/阿里/腾讯）、天气、IP 查询等
- **主题系统** — Mocha 暗色 / Latte 亮色 / 跟随系统等
- **国际化** — 中文 / English
- **右键菜单** — 根据结果类型智能推荐动作
- **高度可配** — 自定义快捷键、引擎开关、开机自启、代理、日志级别等

### 愿景 · 我们要去哪

Blink 不只是一个启动器——它是 **Universal Action Layer（统一操作层）**。以下是正在路上的能力：

- 🎤 **语音输入** — VAD + 语音识别，用说的代替打字
- 🤖 **AI 意图** — 用自然语言描述你想做什么，Blink 自动匹配动作
- 🧠 **上下文感知** — 知道你在哪个应用、选中了什么文字，在你输入之前就推荐正确动作
- ⚡ **主动建议** — 零输入，在对的时间给出对的动作

---

## 安装

从 [Releases](../../releases) 下载最新安装包。

---

## 使用

1. 启动后 Blink 常驻系统托盘
2. **`Alt + Space`** → 唤起输入框
3. 输入应用名（支持拼音首字母，如 `wx` → 微信）
4. 输入算术表达式（如 `1+1`、`100*0.25`）
5. 输入触发词使用插件（如 `fy hello` → 翻译）
6. **↑↓** 选择结果，**回车** ，或者 **Alt + 数字键** 快捷触发 启动 / 复制
7. **Esc** 或点击其他地方 → 隐藏

### 快捷键速查

| 操作 | 说明 |
|---|---|
| `Alt + Space` | 唤起 / 隐藏 Blink |
| `↑` `↓` | 上下选择搜索结果 |
| `Alt + 1~9` | 快捷触发对应位置的结果 |
| `Enter` | 启动选中项 / 复制结果 |
| `PgUp` `PgDn` | 翻页 |
| `Esc` | 隐藏 Blink |

---

## 开发

### 环境要求

- [Rust](https://www.rust-lang.org/tools/install) 1.75+
- [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)（MSVC C++ 工作负载）
- [WebView2 Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/)（Windows 10/11 通常已预装）

### 构建运行

```bash
# 开发模式（debug，控制台日志）
cargo run

# 打包发布
cargo install tauri-cli
cargo tauri build

# 运行测试
cargo test --bin blink
```

---

## 路线图

- ✅ **0.1** — 基础交互（热键/窗口/焦点/IME）+ 搜索 + 配置系统
- ✅ **0.2** — 服务化架构 + 多路搜索 + 搜索缓存
- ✅ **0.3** — 插件系统骨架 + 热键物理态重构
- ✅ **0.4** — 意图路由 RuleRouter + Context 环境感知
- ✅ **0.5** — 统一配置 + 文件搜索（Everything）+ 右键菜单 + 主题 + i18n
- ✅ **0.6** — 插件打包 + Python/Node.js 脚本支持
- ✅ **0.7** — 插件生态 + 本地搜索 Fallback + 性能统计
- 📋 **0.8** — 意图路由优化 + AI Provider 抽象
- 🔮 **0.9+** — 插件市场 · 深度 Context 感知 · Proactive 主动建议

---

## 致谢

- [Wox](https://github.com/Wox-launcher/Wox) — Windows 上的先驱 Launcher
- [Alfred](https://www.alfredapp.com/) — 证明了全局输入框可以成为人机交互的第一入口
- [Raycast](https://www.raycast.com/) — 现代化 Launcher 体验，插件生态的标杆
- [uTools](https://u.tools/) — 国产效率工具，本地化体验的参考
- [Flow Launcher](https://www.flowlauncher.com/) — Windows 上的开源 Launcher，社区驱动的典范
- [Everything](https://www.voidtools.com/) — 极速文件搜索，Blink 文件搜索的集成方案

---

## 许可

[MIT](LICENSE)
