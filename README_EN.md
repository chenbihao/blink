<h1 align="center">Blink</h1>

<p align="center">
  <strong>A Universal Action Layer for Windows — senses what you're doing, makes every action faster.</strong>
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

[View the demo GIF](docs/images/blink.gif)

---

## What is Blink

Blink isn't a launcher — it's a **Universal Action Layer**:
search is just one entry point, **action execution is the destination**. By sensing context and proactively suggesting actions, sequences like "select English → translate", "copy URL → open link", "screenshot → clipboard" go from **multiple steps** to **zero input or one Tab**.

- ⚡ **Fast** — Summon &lt; 50ms, first result &lt; 20ms
- 🎯 **Context-aware** — Auto-senses selection / clipboard / foreground app; unobtrusive but always ready
- 🎤 **Voice Input** — Hold Alt+Space to talk, release to type; supports cloud (OpenAI/Groq) and local (FunASR) engines
- 🧩 **Extensible** — Sandboxed plugin processes (Rust / Python / Node.js / PowerShell); crashes don't affect core

---

## Core Capabilities

### Instant Search

- **App Search** — Scans Start Menu; fuzzy match + Pinyin initials + full Pinyin (`wx` → 微信, `dksz` → 打开设置)
- **File Search** — Integrates Everything; falls back to local directory scan when not installed
- **Live Calculator** — Type `1+1` or `100*0.25`, press Enter to copy
- **Clipboard History** — Background auto-record + fuzzy search history

### Context Awareness

- **Selection Sensing** — Grabs selected text in other apps; one-key translate/search after summoning
- **Ghost Text Autocomplete** — Type `fy hello`, see suggested `fanyi hello`; press Tab to trigger translation
- **Smart Suggestions** — Clipboard is URL / file path → auto-suggest "Open link / Reveal in Explorer"; select English text → Ghost suggestion to translate
- **Non-intrusive by design** — Awareness runs silently in the background; useful when you see it, invisible when you don't; each feature can be toggled individually in settings

### Chord Quick Actions

Hold Alt while the main window is open; letter keys trigger actions directly:

| Combo | Action |
|---|---|
| `Alt + 1~9` | Quick launch item by position in results |
| `Alt + C` | Clipboard history |
| `Alt + A` | Region screenshot (area capture → clipboard / save) |
| Hold `Alt + Space` | Voice input (release to type) |

### Voice Input

Hold **Alt+Space** to talk, release to type — no need to switch IME or type manually.

- **Two modes**: Main window voice input (text fills the search box) and **Voice Typing** (text goes directly to the current cursor position, like typing by voice)
- **Dual engines**: Cloud (OpenAI Whisper / Groq, billed by audio duration) and Local (FunASR SenseVoice, offline & free, 17× real-time on CPU, ~92% accuracy)
- **Pseudo-streaming**: See words appear as you speak — VAD auto-splits sentences and finalizes text, no need to wait until you finish the whole sentence
- **Privacy**: Local mode is fully offline, no audio persistence; cloud mode sends audio to the provider

### Plugin Ecosystem

- **Isolated Processes** — JSONL protocol; crashes don't affect core. Runtimes: Rust / Python / Node.js / PowerShell
- **Built-in Plugins** — Translation (Youdao / Baidu / DeepL / Ali / Tencent), Weather, IP lookup
- **Flexible display modes** — Plugins can mix into results, pin to top, or take over the entire return area

### Polish

- **Context Menu** — Smart actions by result type (open location, copy path, run as admin, etc.)
- **Themes** — Catppuccin Mocha dark / Latte light / Follow system
- **i18n** — Full 中文 / English bilingual (including plugin content)
- **Configurable** — Hotkeys, engine toggles, auto-start, proxy, log levels, per-item context awareness toggles

---

## Roadmap

**Done:** Core search → Plugin ecosystem → Context awareness → Chord interactions → AI conversation capabilities → Frontend architecture overhaul → Voice input

**Next:**

| Direction | Content | Status |
|---|---|---|
| 🔌 **Local & Ecosystem** | On-demand local models · skill-ification · bidirectional MCP · RAG memory | 📋 Planned |
| 🛡️ **Trust Boundary** | AI only recommends actions, never executes directly; confirmation always stays in your hands | ✅ Built-in |

---

## Installation

Download the latest installer from [Releases](../../releases).

---

## Usage

1. Blink sits in the system tray after launch
2. **`Alt + Space`** → summon the input bar
3. Type app name (Pinyin initials/full supported), math expression, or plugin trigger word
4. **↑↓** to navigate, **Enter** or **Alt + 1~9** for quick launch
5. While main window is open, **hold Alt** → see quick action hints
6. **Esc** or click outside → hide

### Keyboard Shortcuts

| Action | Description |
|---|---|
| `Alt + Space` | Summon / hide |
| `↑` `↓` | Navigate results |
| `Alt + 1~9` | Quick launch item by position in results |
| `Tab` | Accept completion suggestion (e.g. `fy` → `fanyi `) or context recommendation |
| `Enter` | Launch / copy result |
| `PgUp` `PgDn` | Page up/down |
| `Alt + C` | Chord: clipboard history |
| `Alt + A` | Chord: region screenshot |
| Hold `Alt+Space` | Voice input (release to type) |
| `Esc` | Hide |

---

## Development

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) 1.75+
- [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) (MSVC C++ workload)
- [WebView2 Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) (pre-installed on Windows 10/11)

### Build & Run

```bash
# Dev mode (debug, console logging)
cargo tauri dev

# Release build (includes plugin compilation)
cargo xtask release

# Tests
cargo test --bin blink
```

### Want to modify the core?

Read [`docs/production-design/`](docs/production-design/README.md) first:
- [`00-overview.md`](docs/production-design/00-overview.md) — Product vision + milestones
- [`product-platform.md §5.0`](docs/production-design/product-platform.md) — Four-domain architecture rules
- [`phases/{version}-*.md`](docs/production-design/phases/) — Design decisions and pitfalls per version

---

## Special Thanks

- [Wox](https://github.com/Wox-launcher/Wox) — The launcher that started it all on Windows
- [Alfred](https://www.alfredapp.com/) — Proved that a global input box can be the first entry for human-computer interaction
- [Raycast](https://www.raycast.com/) — Modern launcher experience, gold standard for plugin ecosystems
- [uTools](https://u.tools/) — Reference for localized experience on Chinese desktops
- [Flow Launcher](https://www.flowlauncher.com/) — Community-driven open-source launcher on Windows
- [Everything](https://www.voidtools.com/) — Lightning-fast file search, integrated as Blink's file search backend
- [Quicker](https://getquicker.net/) — Inspiration for the Chord interaction pattern

---

## License

[MIT](LICENSE)
