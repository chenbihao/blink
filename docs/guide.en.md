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
9. [Using plugins](#9-using-plugins)
10. [Hidden gems at a glance](#10-hidden-gems-at-a-glance)
11. [Keyboard shortcuts reference](#11-keyboard-shortcuts-reference)

---

## 1. Quick start

| Operation | Trigger | Notes |
|---|---|---|
| Summon / hide main window | `Alt+Space` (tap) | Keybinding is remappable in Settings |
| Search | Just type | Fuzzy matching + pinyin initials + full pinyin (`wx` finds WeChat, `dksz` finds Open Settings — Chinese entries match by pinyin); never disturbs you while an IME is composing |
| Pick a result | `↑` `↓` / mouse hover | Shifts the viewport one item at a time when crossing its edge |
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

Every Chord action can be remapped in Settings and promoted to a **global hotkey** (works even when the main window is hidden). Custom combos support Ctrl/Alt/Win (Shift optional) + letter/number/space/punctuation/arrow/PgUp-PgDn-Home-End/F1–F11, or standalone F1–F11; invalid recordings show the exact reason in the status line.

---

## 2. Main window and search

| Operation | Trigger | Notes |
|---|---|---|
| Page | `PgUp` / `PgDn` | Always moves a full screen, preserving the relative highlight position |
| Wheel navigation | Scroll in the main window | Search and clipboard share the same preference, defaulting to line by line. Choose full-page scrolling in Settings → General → Main window wheel behavior; changes apply immediately and preserve the viewport and selection |
| Dual-slot suggestions | `Tab` / `Shift+Tab` | Primary and secondary suggestions each own one key; you can also click the suggestion row |
| Command mode | Start input with `>` | Exclusive mode; `Enter` runs the command in a terminal; `Backspace` past `>` exits |
| Calculator | Just type an expression | An `= result` item appears; `Enter` copies the result |
| Clipboard recall | Type `剪贴板` / `clip` / `jtb` | Opens the clipboard history without `Alt+C` |
| Web links | `bili` + Enter opens Bilibili's homepage; `bili keywords` searches | Keywords support Tab completion. No query opens the homepage; a query replaces `{query}` in the search URL. Set a homepage and optional search URL in Settings → Search Engines → Web Links. An empty homepage automatically uses the search site's home address. The list sorts by name A–Z (Chinese names by pinyin), filters by name, keyword or URL, long lists scroll inside the card with a total count; Click "Import from browser" to import custom search engines from Chrome / Edge / Brave in one step (preview and tick entries first; keywords are editable and conflicts are flagged with reasons). Edit in a separate dialog; conflicts keep your draft for correction. Default GitHub supports `gh` for its homepage and `gh keywords` for search |
| Browser bookmarks | Type `bm`; `bm keywords` to search | Searches Chrome / Edge / Brave bookmarks (title/URL/folder path, pinyin initials supported); `bm` lists recent bookmarks, and Tab completes the keyword for query input. Results show their source browser's icon, falling back to a bookmark icon if unavailable; Enter still opens in the default browser. Bookmarks are indexed in local memory only; keyword, browser scope and mixing live in Settings → Search Engines → Browser Bookmarks |
| File search | Type a file-name fragment | Searches your whole disk. Already running Everything? Enter its port under Settings → Search Engines → File Search to share it. No Everything? Install the built-in engine from the same card (~1.8 MB; it starts automatically after install). Blink picks whichever source is currently available — no manual switching needed |
| New version notice | Suggestion bar on an empty window; type `update` (or `更新`) | Blink silently checks for new versions once a day. When one is found, the suggestion bar shows "Blink x.y.z available" — `Tab` opens Settings → About with the changelog and one-click update; click the `×` at the end of the row to dismiss it for this version (it returns when a newer release arrives). The tray menu also has "Check for Updates"; auto-check can be turned off in Settings → About, manual checking still works |
| Idle context suggestions | Summon after copying a URL / after selecting text anywhere | The suggestion bar offers "open link / ask AI / translate selection", labeled with where the content came from |
| Context menu | Right-click anywhere | Cut/copy/paste/select all, Chord shortcuts, create a sticky note from the search text, note manager, settings, exit |
| Result item menu | Right-click a result entry | Open containing folder, copy path / file name, open shortcut target, reset that entry's launch-frequency stats, and more |
| Drag the main window | Drag the card's blank area | Moves the main window (no visual hint, but the blank area really is draggable) |

### Silky tricks

| Trick | Trigger | Notes |
|---|---|---|
| Color detection | Type `#fff`, `rgb(...)`, `hsl(...)` | A color swatch item appears; `Enter` copies HEX; right-click for HEX/RGB/HSL or pin it as a sticky note |
| Custom search shortcuts | `bil` → Tab → `bili ` | Shares Smart Tab with plugins: Tab accepts the primary suggestion, Shift+Tab the secondary. Pinyin is only a matching alias: `jt` suggests 剪贴板, and Tab inserts the real Chinese keyword. Full keywords retain a quiet Tab hint; accept and continue typing a query. Repeated refreshes do not hide completion. Use `{query}` in the search URL; text and symbols are encoded automatically. `bili` + Enter opens the homepage. Set a homepage manually when search uses another origin (default Bilibili already does) |
| Bookmark hover preview | Hover the left third of a bookmark row | Full title, URL and folder path shown in one panel — long titles are no longer clipped by the list |
| Refresh bookmarks now | Settings → Search Engines → Browser Bookmarks → Refresh now | Loads automatically after startup, then follows bookmark changes in your browser with periodic checks; changing browser scope, enabled state or the entry cap triggers an immediate re-scan. Use "Refresh now" after adding bookmarks to re-read right away; the status shows progress and the final count |
| Real-time whole-disk indexing | Settings → Managed Engines → File Search Engine | One card manages the built-in engine’s install, start, stop and uninstall. For true real-time whole-disk indexing, click "Install service" and authorize once. Searching works without the service too — only the initial index build takes a little longer |
| Promote AI thread | Press `Alt+Q` again inside AI mode | Exports the temporary conversation from the main window into a persistent one and opens the chat window |
| AI dictation guard | Hold `Alt+Space` and speak in AI mode | The transcript only fills the input box, **never auto-sends**; confirm, then `Enter` |
| Modifier protection | Press `Alt` / `F1`-`F12` in the main window | Won't trigger the system menu bar or freeze the window — safe to hold `Alt` while reading keycap hints |

---

## 3. Clipboard history

Hover over the **left third** of an entry to preview it: text shows its full content (scrollable; drag-select and press `Ctrl+C` to copy), and images show the original (**scroll to zoom around the pointer, drag after zooming, double-click to reset**). File images support PNG, JPG/JPEG, GIF, and WebP with animations playing; ordinary files show their full path, multi-file entries list their paths, and missing or unsupported items explain why. `Esc` closes the preview.

Right-click inside the preview for that entry's menu: copy, edit, pin as a note, or delete for text; copy, pin, or edit for images; copy path list or delete for files, plus "Save as image entry" with follow-up pin/edit for single image files. Right-clicking a **single text or file entry** also detects open-style actions — a link offers "Open URL", and a local path offers "Open file / Open containing folder".

Open with: `Alt+C` in the main window (or type 剩贴板 / `clip`). Text, images, and copied file paths are recorded in the background (**file content is never stored**; restricted sources such as network paths are saved as paths only).

| Operation | Trigger | Notes |
|---|---|---|
| Filter | Just type | Fuzzy-search your clipboard history |
| Type filter | Top "All / Text / Images / Files" chips, or cycle with `Tab` / `Shift+Tab` | Switching keeps the query, clears the selection, and returns to the first page; each visit starts on "All" |
| Copy entry | Select one entry and press `Ctrl+C`; text/images also offer right-click "Copy" | Supports text, images, and files; paste copied files manually at the destination |
| Paste out | `Enter` / double-click / `Alt+number` | Text is really pasted into the focused-before-summon window's input box (Blink's own windows like chat and sticky notes work too; falls back to copy when the target isn't an input box); images and files go to the clipboard for manual pasting |
| Edit entry | `Alt+E` or right-click "Edit" | Opens the content editor; saving creates a new history entry. Editing a pinned entry moves the pin to the new content (the old entry drops back to normal ordering) |
| Pin entry | Right-click any text/image/file entry | Four durations: pin for 1/6/12/24 hours. Pinned entries stay at the top (the most recently pinned first) with remaining time shown at the row's end, and are exempt from retention/count cleanup. Pin the same duration again to restart the timer and move the entry back to the top, or unpin; expired entries return to normal ordering. The clipboard list stays open after pinning |
| Delete entry | `Alt+D`, the context menu's delete options, or bare `Delete` (with empty input) | Shortcuts delete the selection, or the highlighted entry if none; the context menu lets you choose the scope |
| Image entry menu | Right-click an image item | Pin to desktop, or open the image editor directly (without overwriting the clipboard) |
| File entry | `Enter` / double-click / `Alt+number` / single-select `Ctrl+C` | Writes the whole path list back to the clipboard as files; press `Ctrl+V` in the target app to paste. Replays use **copy semantics**; expired paths still copy |
| File entry menu | Right-click a file item | "Copy path list" (original paths, one per line, as text); a single PNG, JPG/JPEG, or static WebP offers "Save as image entry", "Save as image entry and pin", or "Save as image entry and edit". These save PNG history, keep the file entry, and leave the system clipboard unchanged; GIF has no save-as-image option |
| Frequency weighting | On every paste / copy | The entry rises in the list — the more you use it, the easier to find |

### Multi-select and batch operations (build the habit)

| Operation | Trigger | Notes |
|---|---|---|
| Single / multiple selection | Click / `Ctrl+click` | Text and images can be mixed; Ctrl toggles an entry, plain click replaces the selection |
| Range selection | `Shift+click` | Selects from the anchor through the clicked entry across loaded pages; `Ctrl+Shift+click` adds a range |
| Select all | `Ctrl+A` (with empty input) | Selects all currently loaded history entries, including images |
| Batch copy / paste | `Ctrl+C` / `Enter` with text selected | Joins in selection order with newlines; saves the combination as one new history entry |
| Merge & edit / note | `Alt+E` / `Alt+S` with text selected | Merge editing keeps the originals; note creation produces one note |
| Actions by type | Right-click a selected entry in a mixed selection | Text copy / paste / edit / note come before "Copy selected file paths", then image pinning; deletion always follows "Text only → Images only → Files only → All selected", regardless of selection order |
| Batch pin images | Select images, then right-click "Pin" | Opens images sequentially with offset positions, up to 16 per operation; a failed image does not stop the rest |
| Batch delete | `Alt+D` / `Delete` with empty input | Deletes the selection, or the highlighted history entry if nothing is selected; failed entries stay selected with feedback |
| Right-click selection | Right-click a selected / unselected entry | Keeps the selected group / selects that entry alone; blank space keeps the selection |
| Clear selection | `Esc` | Clears selection first, then exits clipboard mode on the next press |

### Silky tricks

- Multi-selection survives paging and list refreshes; it resets automatically when the window hides, so it never lingers by mistake.
- With mixed selections, `Ctrl+C` / `Enter` / `Alt+E` / `Alt+S` ask you to choose a type from the context menu to avoid silently dropping images; a single image can still be copied.
- Combination entries created by batch copy / paste **keep only the latest one per combination**, auto-pinned to top; repeated use never piles up.
- Selecting multiple file entries and pressing `Ctrl+C` merges their paths as text (in selection order, deduplicated); mixed selections containing files offer no cross-type paste — copy by type from the context menu. File entries appear on "All" and "Files", where you can search by filename or path.
- Filters follow the stored entry type: paths copied as text belong to "Text"; files, folders, and multi-file groups belong to "Files". A PNG recorded as a file entry stays in "Files". "Save as image entry" creates separate history under "Images" and keeps the original file entry.
- Previews show even very large images at original quality; “Save as image entry” handles one image at a time, and very large tall images may exceed its limit — being previewable does not always mean it can be saved.
- GIF and animated WebP are preview-only (saving would lose the animation); JPEG imports are rotated to the shooting orientation automatically.
- Save and pin or edit a file image directly, without switching to "Images" to find it. A failed import stops the following action. If saving succeeds but pinning or editing fails, the saved image entry remains and a notice explains the failure. Error feedback appears at the bottom of the window, above hover previews. If the image editor already has an active session, that editor is brought forward.
- Clipboard history cleans up automatically: text is kept for 180 days by default with a 10,000-entry cap, images up to 1,000 — all adjustable under "Settings → Chord → Clipboard history" (set the retention to 0 for permanent); pinned entries are exempt from automatic cleanup.
- File-path capture and its limit are configurable under "Settings → Chord → Clipboard history"; deleting a file entry removes only the history record and **never touches the original files on disk**.

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
| Sidebar toggle | Menu button on the left of the header | Open by default; remembered within the session, resets after restart |
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

## 9. Using plugins

### Common operations

- Enable a plugin and expand its configuration under **Settings → Plugins**, then type its trigger in the main window. Translate is disabled by default; enable it and configure engine credentials first.
- Ordinary settings save automatically. Weather's “Save location” button commits only the location draft, so click it after choosing a place.
- Weather and Translate credential groups start collapsed each time you open settings. Their headings show Not configured, Incomplete or Configured; expand a group for its registration link. Configured means all fields are present; an actual query checks whether they work. Use “Remove spaces” below an input if it contains whitespace. Password fields are stored in Windows Credential Manager: leaving them blank keeps the saved value; use the side button to clear it.
- Hover over the left third of a query error to read its full message. Right-click the entry or preview and choose “Copy error message” when reporting a problem.

### Weather

| How to call | Action |
|---|---|
| `天气` / `weather` | Query the saved default location |
| `天气 广州` / `weather Guangzhou` | Query another place without changing the default |
| `天气 辽宁/朝阳` / `天气 广州/天河区` | Add a parent region to distinguish places with the same name |

**Common operations**:

- In Weather configuration, click “Change location,” search and select a complete candidate, then click “Save location.” You can also enter latitude and longitude manually; canceling discards the location draft.
- Choose Open-Meteo (free, no key required), QWeather or Caiyun as the “Weather provider”; adjust the temperature unit separately. “Search source” in the location panel only chooses the service used to find places.
- QWeather: obtain an API Key from the [QWeather console](https://console.qweather.com/) and copy your account-specific API Host from console settings. Paste a bare hostname or an `https://` root URL, without an API path.
- Caiyun: obtain an App Key and App Secret from the [Caiyun open platform](https://platform.caiyunapp.com/). App Key is visible text that you can edit or clear; App Secret is stored as a password field. If a query asks you to confirm the location region, choose “Mainland China” or “Outside mainland China” under “Change location,” then save.
- “Allow fallback to other providers” is off by default. Enable it and drag “Fallback order” to try configured providers after the primary fails; results identify the actual source. Partial weather results explain which data is missing.

**Tips**:

- If a Chinese place name returns no matches, try the full district or county name, add a parent region, use an English name, or enter coordinates directly.
- The first row shows current weather, or a weather summary when current conditions are unavailable. Each following forecast row covers one day, showing the date, weekday, conditions, and low–high temperature; its subtitle shows precipitation probability and maximum wind speed.
- Select the first row and press `Enter` to copy the complete weather summary. Select a daily forecast and press `Enter` to copy only that day's forecast. Select the attribution entry at the end and press `Enter` to open the source and usage information.
- Forecast length depends on the service's actual response. A daily description summarizes the day; it does not mean the condition lasts all day.

### Translate

| How to call | Action |
|---|---|
| `翻译 hello` / `translate hello` | Translate text into the configured target language |
| Select the translated result and press `Enter` | Copy the translation |

**Common operations**: Choose “Default engine” and “Target language” in Translate configuration, then expand the engine's credential group to register and enter credentials. Alibaba, Baidu, Tencent, Youdao and DeepL are supported. To switch engines automatically after a failure, enable “Allow fallback to other engines” and drag to reorder them; fallback is off by default.

**Tips**: Set the target language to “Auto-detect” for quick Chinese–English translation: Chinese input becomes English, and other languages become Chinese. Variable names such as `helloWorld` and `hello_world` are split into words before translation.

### IP lookup

**How to call**: Type `ip` to view local addresses and the egress IPs seen by domestic and international detectors. Select an address and press `Enter` to copy it. After typing `ip`, the list shows `ip refresh` / `ip local` guide entries — press `Enter` to run that query directly. Typing `ip r` shows a gray inline completion; press `Tab` to complete it to `ip refresh` (alias prefixes such as `ip 刷` / `ip 本` complete the same way).

**Common operations**: Under **Settings → Plugins → IP**, choose the IPv4 query scope. “Query IPv6” is off by default; enable it to show one valid non-temporary and one temporary local IPv6, and run one independent IPv6 egress check, with IPv4 listed first. “Enhanced network details” is off by default; enable it to also query gateways, tunnel/virtual adapters and system proxy status, and show other valid IPv6 addresses. Drag to reorder detectors. Failed checks use backups within the same group by default; fallback can be disabled. The international IPv4 detectors include location info by default.

**Tips**: Use `ip local` for local addresses without network requests, or `ip refresh` to refresh egress results subject to detector rate limits. When sharing a LAN address, choose the physical adapter's local IPv4. While typing, the query fires after a pause of roughly half a second, so intermediate keystrokes do not produce error rows.

---

## 10. Hidden gems at a glance

Things you would almost never discover without this list:

1. **The main window's blank area is draggable** — there is no drag bar, but holding the card's blank area moves the window.
2. **Shared main window wheel navigation** — search and clipboard scroll line by line by default; choose full-page scrolling in General settings. Arrow keys always move one item; `PgUp/PgDn` always page.
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
18. **Type `更新` (update) to check for updates**.

---

## 11. Keyboard shortcuts reference

### Global (remappable in Settings)

| Keys | Action |
|---|---|
| `Alt+Space` (tap) | Summon / hide the main window |
| `Alt+Space` (hold) | Voice input, release to transcribe, `Esc` to cancel |
| Chord global keys (optional) | Each action can "follow the summon key" or use a custom combo |

### Inside the main window

| Keys | Action |
|---|---|
| `↑` `↓` | Pick a result (shifts the viewport one item at its edge) |
| `Enter` | Open / copy / paste out |
| `Alt+1` ~ `Alt+9` | Trigger the result at that position |
| `PgUp` / `PgDn` | Full-page navigation |
| Wheel | Line by line (choose full-page scrolling in General settings) |
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
