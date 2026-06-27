<p align="center">
  <!-- 🎯 请替换为项目 logo -->
  <img src="icons/logo.png" width="100" alt="Blink Logo"/>
</p>

<h1 align="center">Blink</h1>

<p align="center">
  <strong>Windows 全局快捷入口，更快、更丝滑、更智能。</strong><br/>
  A launcher for Windows. Faster, smoother, smarter.
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
  <!-- 🎯 请替换为实际录屏 GIF -->
  <img src="docs/blink-demo.gif" width="680" alt="Blink Demo"/>
</p>

---

## Features

- **Instant Launch** — `Alt + Space` to summon, < 50ms hotkey response, auto-hide on blur
- **App Search** — Scan Start Menu, fuzzy match + Pinyin initials (`wx` → 微信)
- **File Search** — Everything support with local fallback when Everything is not installed
- **Calculator** — Type `1+1` or `100*0.25`, press Enter to copy result
- **Clipboard History** — Search through your clipboard history
- **Plugin System** — Native (Rust) + Script plugins (Python / Node.js / PowerShell)
- **Built-in Plugins** — Translation (Youdao/Baidu/DeepL/Ali/Tencent), Weather, IP lookup, and more
- **Themes** — Mocha dark / Latte light / Follow system, and more
- **i18n** — 中文 / English
- **Context Menu** — Right-click for smart actions based on result type
- **Configurable** — Custom hotkey, engine toggles, auto-start, proxy, log levels, and more

### Vision · Where We're Heading

Blink isn't just a launcher — it's a **Universal Action Layer**. Here's what we're building toward:

- 🎤 **Voice Input** — VAD + speech-to-text, speak instead of type
- 🤖 **AI Intent** — Describe what you want in natural language, Blink figures out the action
- 🧠 **Context Awareness** — Blink knows what app you're in, what text you selected, and suggests the right actions before you type
- ⚡ **Proactive Suggestions** — Zero input needed, the right action at the right time

---

## Installation

Download the latest installer from [Releases](../../releases).

---

## Usage

1. Blink sits in the system tray after launch
2. **`Alt + Space`** → summon the input bar
3. Type an app name (Pinyin initials supported, e.g. `wx` for 微信)
4. Type a math expression (e.g. `1+1`, `100*0.25`)
5. Type a trigger word for plugins (e.g. `fy hello` → translate)
6. **↑↓** to navigate, **Enter** to launch / copy
7. **Esc** or click outside → hide

---

## Development

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) 1.75+
- [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) (MSVC C++ workload)
- [WebView2 Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) (pre-installed on Windows 10/11)

### Build & Run

```bash
# Dev mode (debug, console logging)
cargo run

# Release build
cargo install tauri-cli
cargo tauri build

# Tests
cargo test --bin blink
```

---

## Roadmap

- ✅ **0.1** — Core interaction (hotkey/window/focus/IME) + search + config
- ✅ **0.2** — Service architecture + multi-engine search + caching
- ✅ **0.3** — Plugin system skeleton + hotkey physical-state refactor
- ✅ **0.4** — Intent router (RuleRouter) + Context layer
- ✅ **0.5** — Unified config + file search (Everything) + context menu + themes + i18n
- ✅ **0.6** — Plugin packaging + Python/Node.js script support
- ✅ **0.7** — Plugin ecosystem + local search fallback + performance stats
- 📋 **0.8** — Intent routing optimization + AI provider abstraction
- 🔮 **0.9+** — Plugin marketplace · Deep context · Proactive suggestions

---

## Special Thanks

- [Wox](https://github.com/Wox-launcher/Wox) — The launcher that started it all on Windows
- [Alfred](https://www.alfredapp.com/) — Proved that a global input box can be the first entry for human-computer interaction
- [Raycast](https://www.raycast.com/) — Modern launcher experience, gold standard for plugin ecosystems
- [uTools](https://u.tools/) — Reference for localized experience on Chinese desktops
- [Flow Launcher](https://www.flowlauncher.com/) — Community-driven open-source launcher on Windows
- [Everything](https://www.voidtools.com/) — Lightning-fast file search, integrated as Blink's file search backend

---

## License

[MIT](LICENSE)
