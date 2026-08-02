<h1 align="center">Blink</h1>

<p align="center">
  <strong>A smooth launcher for Windows — senses what you're doing, makes every action faster.</strong>
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

## More Than a Launcher

**Build an elegant, smooth launcher that seamlessly integrates common features, using Chord mode to invoke various enhanced capabilities — it's more than just a launcher.**

Blink transforms multi-step operations like "select English text → translate", "copy URL → open link", "screenshot → OCR extract text" into a single shortcut or one Tab press. The entire experience revolves around being **smooth** — fast summoning, fast response, short interaction paths.

With **Chord mode** (hold Alt + letter key while the main window is open), you can quickly invoke screenshot, translation, voice input and other enhanced capabilities without switching windows or memorizing complex shortcuts.

Under the hood is a **capability architecture**: every feature (search, screenshot, translation, plugins...) is wrapped as a standard Capability that can be called by AI. In the future, these capabilities will also be available to external AI tools as CLI tools or Skills.

---

## Instant Search

<p align="center">
  <img src="docs/images/feature-search.gif" width="680" alt="Instant Search"/>
</p>

- **App Search** — Fuzzy matching + Pinyin initials + full Pinyin (`wx` → WeChat, `dksz` → Settings)
- **File Search** — Integrates local app scan and Everything extension
- **Live Calculator** — Calculation formulas, press Enter to copy
- **Clipboard History** — Background recording, supports fuzzy search
- **Tab Quick Accept** — Type `fy`, press Tab to auto-complete to `fanyi `, then start typing what you want to translate
- **Lightning Fast** — Summon < 50ms, first result < 20ms

---

## Context Awareness

<p align="center">
  <img src="docs/images/feature-context.gif" width="680" alt="Context Awareness"/>
</p>

Blink silently senses your context in the background, but never intrudes — use it when you see it, forget it when you don't.

- **Selection Sensing** — Select text in any app, summon Blink to auto-capture it, one-key translate or search (a few apps don't expose UIA text interfaces, so selection may not be detected; in those cases, manually `Ctrl+C` to copy and Blink will read the clipboard content)
- **Clipboard Sensing** — Copied a URL? Auto-suggests "open link". Copied English text? Auto-suggests translation via plugins
- **Ghost Text Autocomplete** — Type `fy`, see gray hint `fanyi `, press Tab to accept quickly, then start typing your content
- **Per-item Control** — Each sensing type can be individually toggled in settings

---

## Plugin Ecosystem

<p align="center">
  <img src="docs/images/feature-plugin.gif" width="680" alt="Plugin Ecosystem"/>
</p>

Plugins run in isolated processes — crashes don't affect Blink's core. Supports Python, Node.js, Rust, or any executable.

- **Translation** — Multi-engine built-in
- **Weather** — Clean weather card
- **IP Lookup** — Quick network info check
- **More Possibilities** — Write plugins in your favorite language, JSONL protocol communication, just a few lines of code to integrate
- **Coming Soon** — One-click discovery of CLI tools on your system, auto-configure them as Blink plugins/skills

---

## Chord Enhancements

screenshot:
<p align="center">
  <img src="docs/images/feature-chord-screenshot.gif" width="680" alt="Chord 模式"/>
</p>

AI:
<p align="center">
  <img src="docs/images/feature-chord-ai.gif" width="680" alt="Chord 模式"/>
</p>

other:
<p align="center">
  <img src="docs/images/feature-chord-other.gif" width="680" alt="Chord 模式"/>
</p>



While the main window is open, hold Alt and letter keys become quick action shortcuts. No need to memorize global hotkeys — just glance at the hints when you need them.

| Combo | Action |
|---|---|
| `Alt + Q` | Open AI chat window for conversation |
| `Alt + A` | Region screenshot → OCR / translate / pin / copy / save |
| `Alt + C` | Clipboard history |
| `Alt + Space` | Voice input (supports input in the main window or as a separate voice input method) |
| `Alt + 1~9` | Quick launch item by position in results |

Screenshots support annotation (rectangle, arrow, text, brush, blur, mosaic, etc.), OCR text recognition, on-image translation, and pin-to-top. Hold `Alt + Space` for voice input — supports cloud (OpenAI / Groq) and local (FunASR) dual engines, words appear as you speak with VAD auto-sentence-splitting.

---

## AI and VoiceInput Ready

<p align="center">
  <img src="docs/images/feature-ai.gif" width="680" alt="AI Capabilities"/>
</p>

Every feature in Blink is wrapped as a standard Capability that can be called by AI:

- **Capability Invocation** — AI can call Blink's built-in capabilities (search, open apps, execute commands...)
- **Plugin Invocation** — AI can call any installed plugin's features
- **Multi-provider Support** — Built-in presets for OpenAI, DeepSeek, etc. Configure any compatible provider or local LLM (ollama / lmstudio)
- **Chat Window** — Independent Agent window, natural language command
- **Future Openness** — These capabilities will gradually be available to external AI tools as CLI, MCP, or Skills

---

## Roadmap

**Completed:** Core search → Plugin ecosystem → Context awareness → Chord interactions → Voice input → Screenshot annotation → AI basic call chain

**Next:**

- **Feature Completion** — Improve screenshot capability, clipboard capability, add preview capability, etc.
- **Memory** — Let AI remember your preferences and history, getting smarter over time

---

## Installation

Download the latest installer from [Releases](../../releases).

---

## Usage

1. Blink sits in the system tray after launch
2. `Alt + Space` to summon the input bar
3. Type app name (Pinyin initials/full Pinyin supported), expression, or plugin trigger word
4. `↑` `↓` to select, `Enter` or `Alt + 1~9` for quick launch
5. While main window is open, hold `Alt` to see Chord quick actions
6. `Esc` or click outside to hide

### Keyboard Shortcuts

| Action | Description |
|---|---|
| `Alt + Space` | Summon / hide |
| `↑` `↓` | Navigate results |
| `Tab` | Accept completion suggestion (e.g. `fy` → `fanyi `) or context recommendation |
| `Enter` | Launch / copy result |
| `Alt + Q` | AI chat window (coming soon) |
| `Alt + A` | Region screenshot |
| `Alt + C` | Clipboard history |
| `Alt + 1~9` | Quick launch by position |
| `Alt + Space` (hold) | Voice input |
| `Esc` | Hide |

---

## Development

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) 1.75+
- [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) (MSVC C++ workload)
- [WebView2 Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) (pre-installed on Windows 10/11)

### Build & Run

```bash
# Dev mode
cargo tauri dev

# Release build (includes plugin compilation)
cargo xtask release

# Run tests
cargo test --bin blink
```

### Architecture Overview

```
┌─────────────────────────────────────────────────────┐
│  Frontend (WebView2)                                │
│  Pure HTML/CSS/JS, no build step                    │
└──────────────────────┬──────────────────────────────┘
                       │ invoke / event
┌──────────────────────┴──────────────────────────────┐
│  Tauri Commands Layer                               │
│  IPC entry, connects frontend and backend           │
└──────────────────────┬──────────────────────────────┘
                       │
┌──────────────────────┴──────────────────────────────┐
│  Domain Layer (Four-domain Architecture)            │
│                                                     │
│  Awareness    ── Context sensing (clipboard/        │
│                 selection/foreground app)            │
│  Suggestion   ── Generate suggestions (Ghost Text   │
│                 / smart recommendations)             │
│  Routing      ── Route decisions (Query → best      │
│                 action)                             │
│  Execution    ── Execute actions (unified entry,    │
│                 explicit trigger)                    │
│                                                     │
│  SearchEngine ── Search engines (app/file/          │
│                 calculator/clipboard)                │
│  Capability   ── Capability layer (screenshot/OCR/  │
│                 translation/plugins...)              │
│  AI Provider  ── AI interface (rig-core, switchable │
│                 providers)                          │
└──────────────────────┬──────────────────────────────┘
                       │
┌──────────────────────┴──────────────────────────────┐
│  Infrastructure Layer                               │
│  SQLite / Win32 API / Platform abstraction /        │
│  Plugin process management                          │
└─────────────────────────────────────────────────────┘
```

### Want to dive deeper?

- [`docs/README.md`](docs/README.md) — Documentation hub (product vision, milestones, navigation)
- [`docs/product.md`](docs/product.md) — Product decisions (why it's designed this way)
- [`docs/specs/`](docs/specs/) — Cross-cutting specs (how · hard rules)
- [`docs/phases/`](docs/phases/) — Design decisions and pitfalls per version

---

## Special Thanks

- [Wox](https://github.com/Wox-launcher/Wox) — The pioneer launcher on Windows
- [Alfred](https://www.alfredapp.com/) — Proved that a global input box can be the first entry for human-computer interaction
- [Raycast](https://www.raycast.com/) — Modern launcher experience, gold standard for plugin ecosystems
- [Flow Launcher](https://www.flowlauncher.com/) — Open-source launcher on Windows, a model of community-driven development
- [uTools](https://u.tools/) — Homegrown efficiency tool, reference for localized experience
- [Quicker](https://getquicker.net/) — A feature-rich all-in-one, a source of inspiration
- [Everything](https://www.voidtools.com/) — Lightning-fast file search, Blink's file search integration
- [Ditto](https://github.com/sabrogden/Ditto) — A super handy clipboard management tool
- [QuickLook](https://github.com/QL-Win/QuickLook) — Quick file preview
- [PixPin](https://pixpin.com/) — I'd call it the most powerful screenshot app

---

## License

[MIT](LICENSE)
