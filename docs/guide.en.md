# Blink Tips & Tricks

> This is the Blink user guide: organized by scenario, covering the everyday operations of each feature along with the silky-smooth tricks you would almost never discover on your own.
>
> In-app entry: Settings → Perception & Interaction → Tips & Tricks (the content is embedded in the installed package and always matches your installed version's behavior). The online edition follows the repository and may describe features you have not installed yet.
>
> 中文版：[guide.md](guide.md)（中文版为真源，本文更新可能滞后）。

---

## Contents

1. [Quick start](#1-quick-start)
2. [Main window and search](#2-main-window-and-search)
3. [Clipboard history](#3-clipboard-history)
4. [Screenshot and annotation](#4-screenshot-and-annotation)
5. [Pin to desktop](#5-pin-to-desktop)
6. [Sticky notes and content editor](#6-sticky-notes-and-content-editor)
7. [AI chat](#7-ai-chat)
8. [Voice input](#8-voice-input)
9. [Hidden gems at a glance](#9-hidden-gems-at-a-glance)
10. [Keyboard shortcuts reference](#10-keyboard-shortcuts-reference)

---

## 1. Quick start

| Operation | Trigger | Notes |
|---|---|---|
| Summon / hide main window | `Alt+Space` (tap) | Keybinding is remappable in Settings |
| Search | Just type | Fuzzy matching + pinyin initials + full pinyin (`wx` finds WeChat, `dksz` finds Open Settings — Chinese entries match by pinyin); never disturbs you while an IME is composing |
| Pick a result | `↑` `↓` / mouse hover | Auto page-turn across page boundaries |
| Open result | `Enter` / click | Launches apps, copies, or runs actions depending on the result type |
| Quick trigger | `Alt+1` ~ `Alt+9` | Directly activates the Nth item on the current page (hold `Alt` in the main window to show a number badge on each item) |
| Accept completion | `Tab` | Type `fy` and a ghost `fanyi ` appears; press `Tab` to accept and start typing your translation right away |
| Hide window | `Esc` / click anywhere outside | Auto-hides on focus loss (except while AI is generating) |

While the main window is open, **hold `Alt`** — the input box (when empty) or the bottom bar (when you have typed something) shows the currently available Chord action keycaps. Nothing to memorize:

| Combo | Action |
|---|---|
| `Alt+Q` | AI chat window (text in the main window's input is carried over into the chat input) |
| `Alt+A` | Region screenshot (empty input only) |
| `Alt+C` | Clipboard history (empty input shows everything; with input, your text becomes the filter word) |
| `Alt+E` | Edit current content (priority: result item > input box > system selection > blank editor) |
| `Alt+S` | Pin as sticky note (takes the input box text; empty input creates a blank note) |
| `Alt+Space` (hold) | Voice input; release to transcribe |

Every Chord action can be remapped in Settings and promoted to a **global hotkey** (works even when the main window is hidden).

---

## 2. Main window and search

| Operation | Trigger | Notes |
|---|---|---|
| Page | `PgUp` / `PgDn`, or the **mouse wheel** over the results | Wheel up = previous page, down = next page; keeps your relative position within the page |
| Dual-slot suggestions | `Tab` / `Shift+Tab` | Primary and secondary suggestions each own one key; you can also click the suggestion row |
| Command mode | Start input with `>` | Exclusive mode; `Enter` runs the command in a terminal; `Backspace` past `>` exits |
| Calculator | Just type an expression | An `= result` item appears; `Enter` copies the result |
| Clipboard recall | Type `剪贴板` / `clip` / `jtb` | Opens the clipboard history without `Alt+C` |
| Idle context suggestions | Summon after copying a URL / after selecting text anywhere | The suggestion bar offers "open link / ask AI / translate selection", labeled with where the content came from |
| Context menu | Right-click anywhere | Cut/copy/paste/select all, Chord shortcuts, create a sticky note from the search text, note manager, settings, exit |
| Result item menu | Right-click a result entry | Open containing folder, copy path / file name, open shortcut target, reset that entry's launch-frequency stats, and more |
| Drag the main window | Drag the card's blank area | Moves the main window (no visual hint, but the blank area really is draggable) |

### Silky tricks

| Trick | Trigger | Notes |
|---|---|---|
| Color detection | Type `#fff`, `rgb(...)`, `hsl(...)` | A color swatch item appears; `Enter` copies HEX; right-click for HEX/RGB/HSL or pin it as a sticky note |
| Promote AI thread | Press `Alt+Q` again inside AI mode | Exports the temporary conversation from the main window into a persistent one and opens the chat window |
| AI dictation guard | Hold `Alt+Space` and speak in AI mode | The transcript only fills the input box, **never auto-sends**; confirm, then `Enter` |
| Modifier protection | Press `Alt` / `F1`-`F12` in the main window | Won't trigger the system menu bar or freeze the window — safe to hold `Alt` while reading keycap hints |

---

## 3. Clipboard history

Open with: `Alt+C` in the main window (or type `剪贴板` / `clip`). Text and images you copy are recorded in the background.

| Operation | Trigger | Notes |
|---|---|---|
| Filter | Just type | Fuzzy-search your clipboard history |
| Type filter | Top "All / Text / Image" chips, or cycle with `Tab` / `Shift+Tab` | Switching type clears the multi-selection |
| Copy entry | Select one entry and press `Ctrl+C`, or right-click "Copy" | Works for text and images; the single-entry context menu shows `Ctrl+C` |
| Paste out | `Enter` / double-click / `Alt+number` | Text is really pasted into the focused-before-summon window's input box (falls back to copy when the target isn't an input box); images go back to the clipboard |
| Edit entry | `Alt+E` or right-click "Edit" | Opens the content editor; saving creates a new history entry |
| Delete entry | `Alt+D`, the context menu's delete options, or bare `Delete` (with empty input) | Shortcuts delete the selection, or the highlighted entry if none; the context menu lets you choose the scope |
| Image entry menu | Right-click an image item | Pin to desktop, or open the image editor directly (without overwriting the clipboard) |
| Frequency weighting | On every paste / copy | The entry rises in the list — the more you use it, the easier to find |

### Multi-select and batch operations (build the habit)

| Operation | Trigger | Notes |
|---|---|---|
| Single / multiple selection | Click / `Ctrl+click` | Text and images can be mixed; Ctrl toggles an entry, plain click replaces the selection |
| Range selection | `Shift+click` | Selects from the anchor through the clicked entry across loaded pages; `Ctrl+Shift+click` adds a range |
| Select all | `Ctrl+A` (with empty input) | Selects all currently loaded history entries, including images |
| Batch copy / paste | `Ctrl+C` / `Enter` with text selected | Joins in selection order with newlines; saves the combination as one new history entry |
| Merge & edit / note | `Alt+E` / `Alt+S` with text selected | Merge editing keeps the originals; note creation produces one note |
| Actions by type | Right-click a selected entry in a mixed selection | Text copy / paste / edit / note come before image pinning; deletion always follows "Text only → Images only → All selected", regardless of selection order |
| Batch pin images | Select images, then right-click "Pin" | Opens images sequentially with offset positions, up to 16 per operation; a failed image does not stop the rest |
| Batch delete | `Alt+D` / `Delete` with empty input | Deletes the selection, or the highlighted history entry if nothing is selected; failed entries stay selected with feedback |
| Right-click selection | Right-click a selected / unselected entry | Keeps the selected group / selects that entry alone; blank space keeps the selection |
| Clear selection | `Esc` | Clears selection first, then exits clipboard mode on the next press |

### Silky tricks

- Multi-selection survives paging and list refreshes; it resets automatically when the window hides, so it never lingers by mistake.
- With mixed selections, `Ctrl+C` / `Enter` / `Alt+E` / `Alt+S` ask you to choose a type from the context menu to avoid silently dropping images; a single image can still be copied.
- Combination entries created by batch copy / paste **keep only the latest one per combination**, auto-pinned to top; repeated use never piles up.

---

## 4. Screenshot and annotation

Summon: `Alt+A` in the main window (empty input; can be promoted to a global hotkey in Settings).

| Operation | Trigger | Notes |
|---|---|---|
| Smart pre-selection | Hover a window / control | Blue dashed box + spotlight cutout; click to adopt that region (clicking within 10px of a window's edge always adopts the whole window) |
| Free-form selection | Left-button drag | Live `(x, y) w × h` size label |
| Adjust region | Drag the 8 edge handles / drag inside the region | Resize / move (clamped to the monitor) |
| Toolbar | Appears automatically after selection | Shapes (rectangle/ellipse/spotlight/magnifier), pens (pencil/arrow/highlighter), text (text/numbered badge/watermark), mosaic/Gaussian blur, eraser |
| Undo / redo / reset | `Ctrl+Z` / `Ctrl+Y` (or `Ctrl+Shift+Z`) / `Ctrl+R` | Annotation-command-level undo; while editing text, undo/redo applies to the text |
| Output | Copy / save / pin / OCR / translate / translate & pin (see §5) | Double-click inside the region to confirm copying the screenshot, including over OCR text; after OCR, translate on the image or drag-select text to copy |
| Scrolling capture | Toolbar "Scrolling capture" | Scrolls the target window and stitches frames; supports "auto scroll"; enters annotation mode when done |
| Cancel | `Esc` / `✕` / right-click without a selection | Layered closing: OCR panel / subpanels / dropdowns first, then the whole layer |

### Silky tricks

| Trick | Trigger | Notes |
|---|---|---|
| Shortcut badges | **Hold `Alt` / `Ctrl`** | `Alt` shows tool-switching badges; `Ctrl` shows Z/Y/R badges for undo / redo / reset |
| Cycle within group | Repeat `Alt+1`~`Alt+5` | Cycles tools within the group and briefly expands the dropdown to show the current item |
| Wheel tunes everything | Wheel over a tool group button / thickness step / intensity·font-size slider / hue·opacity bar / preset swatches / color format | Switches tools, adjusts thickness, steps sliders, tunes color, switches formats respectively |
| Brush size on the canvas | With the pencil or a brush-mode tool selected, wheel inside the region | Scroll up to increase size, down to decrease it; the cursor outline updates immediately with a brief size readout. Use presets or the slider for fine adjustment. Size stays fixed during a stroke; adjust after releasing. Existing annotations keep their size. During scrolling capture, the wheel still scrolls the target window |
| Square constraint | Hold `Shift` while selecting / drawing rectangles or ellipses | Ratio constraint, applied live on press and release |
| `E` key | Press `E` while annotating | Toggles the OCR panel |
| Double-click full-screen copy | Double-click the dim overlay without a selection | Copies the whole screen |
| Right-click has three meanings | No selection = cancel / annotating = copy region / selecting text = text menu | One right-click, three semantics |
| Silent text layer | OCR runs in the background right after you select | Drag-select text, then press `Ctrl+C` to copy; right-click can copy all text. Double-click still confirms copying the screenshot. Recognition does not open the panel automatically; press `E` when needed |
| Pixel magnifier | Follows automatically during selection | Built-in hints: `Shift` toggles HEX/RGB/HSL, `C` copies the color |
| Eyedropper | "Pick from screenshot" in the color panel | Full-screen click sampling; arrow keys fine-tune by 1px; `Esc` exits only the picker, not the screenshot |
| Arrow-key nudge | Arrow keys in annotation mode | Moves the region by 1 physical pixel |
| Text commit | Press `Esc` in the text input box | With content it commits (nothing lost); press again to exit |
| Palette extraction | Toolbar "Extract palette" | `Ctrl+click` multi-selects colors, right-click copies one, generates pairing suggestions; "copy group" right-click can output as CSS variables |
| Pan long images | `Space+drag` or middle-button drag on long images / in edit mode | Pans the extra-tall canvas |
| Click outside the region | Click outside the region while annotating | Exits annotation back to selection — does not cancel the screenshot |

---

## 5. Pin to desktop

Create from: the screenshot toolbar "Pin", the OCR panel "Pin", or right-click "Pin" on a clipboard image. Pin windows are always on top and never appear in the taskbar.

| Operation | Trigger | Notes |
|---|---|---|
| Move | Drag the image | Extra-tall images switch to a dedicated drag (top edge can leave the screen; the bottom of a long image stays reachable) |
| Zoom | Wheel (`Ctrl+wheel` for big steps) | 0.1~8×, anchored at the top-left corner, briefly shows the percentage |
| Shrink to mini | **Double-click** | Shrinks to a small window with a 160px max side; double-click again to restore (wheel or "reset zoom" also restores) |
| Context menu | Right-click | Edit image / recognize text / translate image / save to clipboard / save as / reset zoom / close (mini state keeps only the last two) |
| Recognize text | Right-click "Recognize text" | A selectable text layer appears on the image; double-click selects the whole line |
| Translate & replace | Right-click "Translate image" | The translation renders in place with a selection layer attached; the menu becomes "Restore original" (restorable within this session only) |

### Silky tricks

| Trick | Trigger | Notes |
|---|---|---|
| Esc priority chain | Press `Esc` repeatedly | Close menu → exit mini → clear text selection (**does not close the pin** — mis-press protection) → close the pin |
| Wheel restores from mini | Wheel while in mini state | Restores the normal state directly |

---

## 6. Sticky notes and content editor

### Sticky notes

| Operation | Trigger | Notes |
|---|---|---|
| New | `Alt+S` (input box text goes into the note; empty input = blank note) / main-window right-click "Create note from search text" / clipboard entry right-click "Pin as note" | You can also create from the note manager |
| Edit | Click the body and type | Live Markdown rendering (`#`, `**`, task lists, etc.), autosave, window title follows the first line |
| Pin / color | Title-bar pin / palette button | 7 note colors |
| Close semantics | `Esc` or `✕` | Empty note = deleted outright; with content = saved and moved to the **recycle bin** (recoverable) |
| Note manager | Right-click menu / tray | Focus, edit, recolor, hide, delete; the recycle-bin tab restores / deletes permanently / empties |
| Font zoom | **`Ctrl+wheel`** | 0.7~2.0× zoom; `Ctrl+middle-click` restores the default |

### Content editor

`Alt+E` opens the current context (clipboard entry / input box / system selection) in a standalone editor window.

| Operation | Trigger | Notes |
|---|---|---|
| Save | `Ctrl+S` | Writes back to the source (note/file/clipboard); saves without closing |
| Dual view | Top "Raw / MD" tabs, switch with `←` `→` | The MD view renders live, with a formatting toolbar |
| Continuous dictation | Microphone button at the bottom | Long-form dictation split into paragraphs; pause / resume |
| AI polish | "Polish this dictation" chip after dictating, or right-click selected text | Produces a candidate card, applied only after confirmation; **applying is a single-transaction replace — one `Ctrl+Z` undoes it all** |
| Draft protection | Automatic | Edits land in drafts; after a crash or a forgotten save, reopening prompts recovery, with a four-way dialog on conflict |
| More output | Top `⋯` menu | Save a copy, copy all, create a note, send to AI chat (prefills only, never sends), recover unsaved drafts |
| Layered exit | `Esc` | In order: close menu → stop dictation → dismiss candidate card → close window (three-way dialog when there are unsaved changes) |

---

## 7. AI chat

Summon: `Alt+Q` (text in the main window's input is carried over automatically — prefills, never sends).

| Operation | Trigger | Notes |
|---|---|---|
| Send / newline | `Enter` / `Shift+Enter` | Streaming output |
| Stop generating | Stop button or `Esc` while generating | Generated content is kept |
| Hide when idle | `Esc` | Hides the window (conversation preserved) |
| Edit & resend | Hover a user message → edit | Editing truncates the subsequent messages and resends |
| Rename | Double-click a conversation in the sidebar | Inline rename |
| Drag to organize | Drag a conversation into a group / drag a group into another | Moves conversations / reorders groups |
| Temporary conversation | Sidebar `ⓘ` button | Never persisted; the header can "promote to persistent" |
| `/skill` command | Type `/skill` in the input box | Picks one of your configured Skills |
| Context details | Hover the capacity ring at the input's right | Context breakdown, memory health, available tools, MCP status |
| Voice input | Hold `Alt+Space` while the chat window is visible | The transcript goes into the chat input |
| Audio transcription | Audio attachment button at the input's left | AI automatically calls the transcription tool |

### Silky tricks

- You can keep typing while generating, but `Enter` won't send — stop first, then send.
- Unfinished history turns collapse into summary bars; click "expand" to read the full text.

---

## 8. Voice input

| Operation | Trigger | Notes |
|---|---|---|
| Speak | **Hold `Alt+Space`**, release to finish | The transcript lands in the current window: main window → search box, searching immediately; chat → input box; editor → appended to the body |
| Cancel | `Esc` while recording | Discards without transcribing |
| Status hint | Breathing tray-icon animation | The visible sign that recording is on |
| Continuous dictation | Microphone button in the content editor | Long-form dictation with pause / resume / collapse into a mini bar, with direct access to settings |

---

## 9. Hidden gems at a glance

Things you would almost never discover without this list:

1. **The main window's blank area is draggable** — there is no drag bar, but holding the card's blank area moves the window.
2. **Main window wheel = paging** — with more than 9 results, the wheel flips a whole page.
3. **Type `>` to run commands** — `Enter` executes in a terminal, no need to open one first.
4. **Type `#fff` to detect colors** — works in both the main window and the clipboard panel; `Enter` copies HEX.
5. **Clipboard mixed selection** — use `Ctrl+click` to toggle entries and `Shift+click` for ranges. Text-only selections support batch copy / paste; mixed selections offer actions by type in the context menu, including batch image pinning.
6. **Clipboard bare `Delete` deletes** — no need to reach for `Alt+D`.
7. **Screenshot shortcut badges follow their modifier** — hold `Alt` for tool-switching hints, or `Ctrl` for undo / redo / reset hints.
8. **The screenshot wheel tunes everything** — wheel inside the region to adjust brush size with a live cursor preview; tool buttons / thickness / sliders / hue bar / preset swatches also respond to the wheel.
9. **Double-click the dim overlay = copy the whole screen.**
10. **Press `E` while annotating to toggle the OCR panel.**
11. **Text becomes selectable right after you select a region** — silent background OCR; `Ctrl+C` copies text straight off the image.
12. **Double-click a pin to shrink it to mini** — double-click again to restore; `Ctrl+wheel` for big zoom steps.
13. **Repeated `Esc` never closes a pin by accident** — the priority chain clears the text selection first and closes the pin last.
14. **Sticky notes: `Ctrl+wheel` zooms the font**, `Ctrl+middle-click` restores.
15. **AI chat: double-click a conversation name to rename**, drag to organize groups.
16. **Press `Alt+Q` again inside AI mode to promote a temporary conversation to persistent**.
17. **The welcome page / Settings can promote every Chord action to a global hotkey** — `Alt+A` screenshots even with the main window hidden.

---

## 10. Keyboard shortcuts reference

### Global (remappable in Settings)

| Keys | Action |
|---|---|
| `Alt+Space` (tap) | Summon / hide the main window |
| `Alt+Space` (hold) | Voice input, release to transcribe, `Esc` to cancel |
| Chord global keys (optional) | Each action can "follow the summon key" or use a custom combo |

### Inside the main window

| Keys | Action |
|---|---|
| `↑` `↓` | Pick a result (auto page-turn across pages) |
| `Enter` | Open / copy / paste out |
| `Alt+1` ~ `Alt+9` | Trigger the result at that position |
| `PgUp` / `PgDn` / wheel | Page |
| `Tab` / `Shift+Tab` | Accept primary / secondary suggestion |
| `Esc` | Exit mode / clear multi-selection / hide window |
| `Alt+Q` `Alt+A` `Alt+C` `Alt+E` `Alt+S` | Chord actions |
| Hold `Alt` | Show Chord keycaps and result number badges |
| Input starting with `>` | Command mode (`Enter` runs in a terminal) |

### Inside the clipboard panel

| Keys | Action |
|---|---|
| `Ctrl+click` | Multi-select entries |
| `Shift+click` / `Ctrl+Shift+click` | Select / add a range |
| `Ctrl+A` | Select all loaded history entries (with empty input) |
| `Ctrl+C` | Batch-copy text / copy a single image; use the context menu for mixed selections |
| `Enter` | Paste text / copy a single image; batch-paste text-only selections |
| `Tab` / `Shift+Tab` | Cycle the type filter |
| `Alt+E` / `Alt+S` | Edit / create a note from text-only selections (merge multiple entries) |
| `Alt+D` / `Delete` | Delete the selection, or the current entry if none; Delete requires empty input |

### Inside screenshot annotation

| Keys | Action |
|---|---|
| `Alt+1` ~ `Alt+5` | Switch tool groups (repeat to cycle within the group) |
| Hold `Alt` | Show shortcut badges |
| `Ctrl+Z` / `Ctrl+Y` (or `Ctrl+Shift+Z`) / `Ctrl+R` | Undo / redo / reset annotations |
| `E` | Toggle the OCR panel |
| `Shift` (while selecting / drawing) | Square / ratio constraint |
| Arrow keys | Nudge the region by 1px (fine-tune the sampler in the eyedropper) |
| `C` / `Shift` (magnifier visible) | Copy color / switch color format |
| Double-click region / overlay | Copy region / copy whole screen |
| `Esc` | Layered exit (panel → submenu → cancel screenshot) |

### Pins / notes / editor / chat window

| Keys | Action |
|---|---|
| Wheel / `Ctrl+wheel` | Pin zoom / big steps; note font zoom |
| Double-click | Pin mini toggle; chat conversation rename |
| `Esc` | Pin priority chain; note close (to recycle bin); editor layered exit; chat stop generating / hide |
| `Ctrl+S` | Editor saves to the source |
| `Enter` / `Shift+Enter` | Chat send / newline |
