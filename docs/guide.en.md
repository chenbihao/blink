# Blink Tips & Tricks

> Find everyday operations, entry points, shortcuts, and hidden tricks by scenario.
>
> In-app entry: Settings → Perception & Interaction → Tips & Tricks.
>
> 中文版：[guide.md](guide.md)

---

## Contents

1. [Quick start](#1-quick-start)
2. [Main window and search](#2-main-window-and-search)
3. [Clipboard history](#3-clipboard-history)
4. [Screenshots and GIFs](#4-screenshots-and-gifs)
5. [Pin to desktop](#5-pin-to-desktop)
6. [Sticky notes and content editor](#6-sticky-notes-and-content-editor)
7. [AI chat](#7-ai-chat)
8. [Voice input](#8-voice-input)
9. [Plugin Center and plugins](#9-plugin-center-and-plugins)
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

Under **Settings → Chord**, remap an action or enable its **global hotkey** to take screenshots, open chat, or create notes while the main window is hidden.

---

## 2. Main window and search

| Operation | Trigger | Notes |
|---|---|---|
| Page / scroll | `PgUp` / `PgDn`; wheel | Wheel scrolling moves one item by default; choose full-page scrolling in General settings |
| Primary / secondary suggestion | `Tab` / `Shift+Tab` | Complete a keyword or use a contextual action in the suggestion bar |
| Run a command | Type `>` followed by a command | Press `Enter` to run it in a terminal |
| Calculator | Type an expression | `Enter` copies the result |
| Website shortcuts | `bili` / `bili query`; `gh` / `gh query` | Open a homepage or search; add shortcuts or import them from a browser under Settings → Search Engines → Web Links |
| Browser bookmarks | `bm` / `bm query` | Browse recent bookmarks or search Chrome / Edge / Brave by title, URL, or folder |
| File search | Type part of a filename | Install the built-in file search engine under Settings → Managed Engines, or use an existing Everything service |
| Check for updates | Type `更新` / `update`, or use the tray menu | View and install updates on the About page |
| Contextual suggestions | Copy a link or select text, then summon Blink | Open links, ask AI, or translate |
| Context menu | Right-click the main window or a result | Reveal files, copy paths, create notes, open Settings, and more |
| Move the window | Drag the card's blank area | Reposition the main window |

### Silky tricks

- Type `#fff`, `rgb(...)`, or `hsl(...)` to inspect a color. `Enter` copies HEX; right-click for other formats or a sticky note.
- Type `bil`, press `Tab` to complete `bili `, then enter your query. Custom search URLs use `{query}` as the placeholder.
- Hover over the left third of a bookmark result for its full title, URL, and folder. Use **Refresh now** in bookmark settings after adding bookmarks.
- Under **Settings → Managed Engines → File Search Engine**, choose **Install service** for a live index across drives.
- Press `Alt+Q` again in the main window's AI mode to move the temporary conversation into a persistent chat window.

### Sync settings across computers

Under **Settings → Storage → Configuration Sync**, enter your WebDAV connection, click **Save & test connection**, then **Sync now**. On the first sync, choose to upload this computer's configuration or use the remote one; resolve conflicts by choosing a side for each item. Everyday settings, shortcuts, and prompts sync; credentials and personal content stay local.

---

## 3. Clipboard history

Press `Alt+C` in the main window, or type `剪贴板` / `clip` / `jtb`. Find text, images, and files you copied earlier.

| Operation | Trigger | Notes |
|---|---|---|
| Search / filter | Type a query; use the type tabs | `Tab` / `Shift+Tab` cycle through All, Text, Images, and Files |
| Preview | Hover over the left third of an entry | Scroll and select text; zoom images with the wheel, drag to pan, double-click to reset; animated images play |
| Copy | `Ctrl+C` or right-click | Copy the entry; files can then be pasted into the destination folder |
| Paste out | `Enter` / double-click / `Alt+number` | Text is pasted into the previous input; images and files are copied for manual pasting |
| Edit / create note | `Alt+E` / `Alt+S` | Send text to the editor or a sticky note |
| Pin an entry | Right-click → pin for 1 / 3 / 7 days | Keep frequently used content at the top, or unpin it |
| Image actions | Right-click an image | Pin, edit, or ask AI |
| File actions | Right-click a file | Copy paths, open or reveal files; save static image files into image history and pin or edit them |
| Delete | `Alt+D` / `Delete` with empty input | Delete the selection or current entry; deleting file history keeps the original files |

### Multi-select and batch actions

| Operation | Trigger | Notes |
|---|---|---|
| Multi-select | `Ctrl+click` | Toggle individual entries, including mixed text, images, and files |
| Range / select all | `Shift+click` / `Ctrl+A` with empty input | `Ctrl+Shift+click` adds a range; select all covers loaded entries |
| Batch copy / paste | Text-only selection, then `Ctrl+C` / `Enter` | Join text with newlines in selection order |
| Merge for editing / note | Text-only selection, then `Alt+E` / `Alt+S` | Continue working with the combined content |
| Mixed selection | Right-click a selected entry | Act on text, files, and images separately; batch-pin images or delete by type |
| Clear selection | `Esc` | First clears multi-selection; press again to leave clipboard mode |

### Silky tricks

- Right-click inside a preview to open that entry's menu. Right-clicking a selected entry keeps the whole selection.
- Selections survive paging. `Ctrl+C` on selected file entries copies their paths as combined text.
- Batch copying or pasting creates a combined history entry; only the latest copy of the same combination is kept.
- Adjust retention under **Settings → Chord → Clipboard History**. The `⏎` marker only indicates a line break; copying preserves real newlines.

---

## 4. Screenshots and GIFs

Summon: `Alt+A` in the main window (empty input; can be promoted to a global hotkey in Settings).

| Operation | Trigger | Notes |
|---|---|---|
| Smart pre-selection | Hover over a window / control, then click | Adopt the highlighted region; click near a window edge to select the whole window |
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
| Brush size on the canvas | Select a brush tool, then scroll inside the region | Scroll up for thicker strokes, down for thinner; the cursor shows the brush width |
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

### GIF recording and editing

Select a screenshot region, click **Record GIF**, choose the frame rate and whether to show the cursor, then click **Start recording**.

| Operation | Entry / shortcut | Notes |
|---|---|---|
| Move the recording region | Drag the selection before recording, or its handle while recording | Position the region where you need it |
| Pause / resume | Recording control bar | Paused time is excluded from the result |
| Stop / cancel | Recording control bar | Stop opens the editor; cancel discards the recording |
| Preview | `Space` to play / pause; `←` / `→` to step | Find the frames you want to edit |
| Trim / delete frames | Set start / end; select frames and press `Delete` | `Shift+click` selects a range; `Ctrl+click` toggles individual frames |
| Adjust pacing | Speed, frame duration, and loop controls | Hold key moments longer; toolbar buttons undo and redo edits |
| Export | Top-right **Export** button | Choose size, quality, and where to save |

Tip: record a smaller region and choose a smaller export size for a lightweight walkthrough.

---

## 5. Pin to desktop

Create from: the screenshot toolbar "Pin", the OCR panel "Pin", or right-click "Pin" on a clipboard image. Pin windows are always on top and never appear in the taskbar.

| Operation | Trigger | Notes |
|---|---|---|
| Move | Drag the image | Extra-tall images switch to a dedicated drag (top edge can leave the screen; the bottom of a long image stays reachable) |
| Zoom | Wheel (`Ctrl+wheel` for bigger steps) | Resize the pin; a brief overlay shows the zoom level |
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
| AI polish | "Polish this dictation," or right-click selected text | Choose polish, shorten, translate, or another preset; review before applying, then undo with one `Ctrl+Z` |
| Recover drafts | Reopening prompts recovery, or use the top `⋯` menu | Find unsaved content |
| More output | Top `⋯` menu | Save a copy, copy all, create a note, send to AI chat (prefills only, never sends), recover unsaved drafts |
| Layered exit | `Esc` | In order: close menu → stop dictation → dismiss candidate card → close window (three-way dialog when there are unsaved changes) |

---

## 7. AI chat

Press `Alt+Q` in the main window to open chat. Existing input is brought along; review it before sending.

| Operation | Entry / shortcut | Notes |
|---|---|---|
| Send / newline | `Enter` / `Shift+Enter` | `Esc` or Stop stops generation; `Esc` hides the window when idle |
| Switch models | Model button beside the input, or `/model` | Choose the model for this conversation |
| Attach content | Attachment button, or `@` at the start of the input | Pick files, audio, recently copied files/images, or current conversation references |
| Ask about an image | Paste or drop an image; use **Ask AI** on a screenshot or clipboard image | Choose an image-capable model, add a question, then send |
| Reusable prompts | Prompt button, or `/prompt` | Insert a prompt, add your content, then send |
| Edit and resend | Hover over a user message → Edit | Revise your question and regenerate |
| Organize conversations | Double-click a name; drag into a group | Rename or move conversations; drag groups to reorder |
| Temporary conversation | Sidebar ⓘ button | Promote it to persistent from the header when you want to keep it |
| Context details | Hover over the capacity ring beside the input | Inspect context usage, available tools, and MCP status |
| Voice input | Hold `Alt+Space` | Fill the chat input with your transcript |

### `/` commands and prompts

Type `/` at the start of the input to open commands. Use `↑` / `↓` to select, `Enter` / `Tab` to adopt, and `Esc` to close the panel.

| Command | Action |
|---|---|
| `/new` | Start a conversation |
| `/model` | Switch models |
| `/prompt` | Insert a reusable prompt |
| `/skill` | Choose a Skill |
| `/tools` / `/mcp` | Inspect available tools and MCP connections |
| `/compact` | Compact the conversation context |

Manage reusable instructions under **Settings → AI Chat Settings → Prompt Library**. Chat prompts appear in `/prompt`; editor prompts appear in the AI polish presets.

### Silky tricks

- The **Thinking** button shows the toggle or effort levels available for your model; **Default** follows the service.
- Keep typing while generating, then stop generation before sending your next message.
- Right-click clipboard text and choose **Send to AI chat**, or use **Ask AI** on an image to bring the content along.
- Unfinished history turns collapse into summary bars; click **Expand** to read the full text.

---

## 8. Voice input

| Operation | Trigger | Notes |
|---|---|---|
| Speak | **Hold `Alt+Space`**, release to finish | The transcript lands in the current window: main window → search box, searching immediately; chat → input box; editor → appended to the body |
| Cancel | `Esc` while recording | Discards without transcribing |
| Status hint | Breathing tray-icon animation | The visible sign that recording is on |
| Continuous dictation | Microphone button in the content editor | Long-form dictation with pause / resume / collapse into a mini bar, with direct access to settings |

---

## 9. Plugin Center and plugins

### Plugin Center

Browse official plugins under **Settings → Plugins → Plugin Center**.

| Operation | Entry | Notes |
|---|---|---|
| Install / update | Corresponding button in Plugin Center | Download plugins; enabling the offline dictionary starts preparing its databases |
| Enable / configure | Installed plugin card | Enable the plugin, adjust options, or enter credentials |
| Run a plugin | Type its trigger in the main window | `Tab` completes keywords; Translate and Dictionary open a window when the bare keyword is submitted |
| Open a window | **Open page** on the plugin card | Useful for repeated translations or lookups |
| Uninstall / restore | Installation row on the plugin card | Keep or clear data; updated bundled plugins can revert to their bundled version |

Ordinary options save automatically; forms with a **Save** button, such as locations, need that click. Leaving a password field blank keeps the saved credential.

### Weather

| Input / action | Result |
|---|---|
| `天气` / `weather` | Query the default location |
| `天气 广州` / `weather Guangzhou` | Check another location for this query |
| `天气 辽宁/朝阳` | Add a parent region to distinguish places with the same name |
| Select a result and press `Enter` | Copy the complete summary from the first row, or one day's forecast from a daily row |

In Weather settings, choose **Change location**, search and select a place, then **Save location**. Choose key-free Open-Meteo, or configure QWeather or Caiyun. Enable fallback and drag the order if you want automatic switching between sources.

### Translate

| Input / action | Result |
|---|---|
| `翻译 hello` / `translate hello` | Quick translation; `Enter` copies the result |
| Submit bare `翻译` / `translate` | Open the translation window |
| `translate hello -> ja` | Translate to Japanese for this query |
| Translate configuration | Set the default engine, target language, and credentials |

Auto translates Chinese to English and other languages to Chinese. Change the direction or engine within the window for the current session. Variable names such as `helloWorld` and `hello_world` are split into words before translating.

### IP lookup

| Input | Result |
|---|---|
| `ip` | Local addresses plus domestic and international egress IPs; `Enter` copies |
| `ip local` | Local addresses only |
| `ip refresh` | Refresh egress lookups |

Type `ip r`, then `Tab` to complete `ip refresh`. Share a physical adapter's local IPv4 for LAN connections; enable IPv6 and enhanced network details in the plugin's settings.

### Offline dictionary

| Input / action | Result |
|---|---|
| `查词 hello` / `dict hello` | Senses, phonetics, word forms, and phrases |
| `查词 苹果` | Chinese-to-English reverse lookup |
| Submit bare `查词` / `dict` | Open the dictionary window; `Enter` queries and `Esc` hides it |
| Double-click an English word in a result | Open a quick lookup card beside the word, with a link to the full entry |

Click a spelling candidate to look it up; copy the word or senses from the entry. Once its databases are ready, lookups work offline. Preparation progress and **Rebuild** live in the plugin's **Datasets** section.

---

## 10. Hidden gems at a glance

1. **Drag the main window by its blank area** — no title bar needed.
2. **Mix clipboard entry types in one selection**: `Ctrl+click` toggles entries; right-click for batch actions by type.
3. **The screenshot wheel tunes everything**: brush size, tools, sliders, and colors. Hold `Alt` / `Ctrl` to see shortcut badges.
4. **Select text directly on a screenshot**, then `Ctrl+C` to copy; `E` opens the OCR panel.
5. **Double-click a pin to shrink it to mini**, again to restore; `Ctrl+wheel` changes a note's font size.
6. **Hold a key GIF frame longer** to explain an important step without recording it again.
7. **Double-click an unfamiliar English word in dictionary results** for a quick lookup card.
8. **`/prompt` inserts reusable instructions; `@` finds recently copied attachments**. Review before sending.
9. **Press `Alt+Q` again in the main window's AI mode** to save the temporary conversation in a separate window.
10. **Give Chord actions global hotkeys** to take screenshots or open chat while the main window is hidden.

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

### GIF editor

| Keys | Action |
|---|---|
| `Space` | Play / pause preview |
| `←` / `→` | Previous / next frame |
| `Shift+click` / `Ctrl+click` | Select a range / toggle individual frames |
| `Delete` / `Backspace` | Delete selected frames |

### Pins / notes / editor / chat window

| Keys | Action |
|---|---|
| Wheel / `Ctrl+wheel` | Pin zoom / big steps; note font zoom |
| Double-click | Pin mini toggle; chat conversation rename |
| `Esc` | Pin priority chain; note close (to recycle bin); editor layered exit; chat stop generating / hide |
| `Ctrl+S` | Editor saves to the source |
| `Enter` / `Shift+Enter` | Chat send / newline |
| `/` / `@` at the start of the input | Chat commands / attachment panel |
