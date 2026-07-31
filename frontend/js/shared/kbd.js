//! 键帽渲染模块（0.8.1 优化）。
//!
//! **动机**：不同平台键名/符号习惯不同——Windows 显 "Ctrl+Tab"，macOS 显 "⌃⇥"；
//! Enter/Shift/Backspace 等键在两个平台都有约定俗成的 Unicode 符号（↵ ⇧ ⌫）。
//! 硬编码在使用侧无法扩展。抽成本模块后：
//! - ghost overlay、设置页快捷键展示、0.8.3 Chord 提示 UI 共用同一套渲染
//! - 未来加 macOS 支持只需改 `PLATFORM_LABELS`，不动调用点
//!
//! **依赖**：无。Unicode 键盘符号（U+21E5 ⇥, U+21B5 ↵, U+21E7 ⇧, U+232B ⌫ 等）
//! 是 IEC 9995-7 标准，Windows Segoe UI Symbol / macOS SF 系统字体原生支持。
//!
//! **用法**：
//! ```
//! import { renderKey, renderCombo } from "./kbd.js";
//! el.appendChild(renderKey("Tab"));           // <kbd class="kbd">⇥ Tab</kbd>
//! el.appendChild(renderCombo("Ctrl+K"));      // <span class="kbd-group"><kbd>Ctrl</kbd><kbd>K</kbd></span>
//! ```

// ── 平台判断 ────────────────────────────────────────────────────────────────
// 简化版：navigator.userAgent 里含 "Mac" 就是 macOS，否则默认 Windows 语义。
// Tauri 上 WebView2 走 Windows UA，macOS WKWebView 走 Mac UA——判定可靠。

const IS_MAC = /Mac|iPhone|iPad|iPod/.test(navigator.platform || navigator.userAgent);

// 键名 → 显示 label 映射 ─────────────────────────────────────────────────
//
// 每个 key 三段元数据：
//   text:    平台常用文字标签，如 "Tab"
//   symbol:  Unicode 键盘符号，如 ⇥（仅 macOS 使用；Windows 上 Segoe UI Symbol
//            对这些字符的字形设计粗糙，直接显示会丑，行业共识是 Windows 用文字）
//   preferSymbol: 该键在**任何平台**是否偏好符号（方向键就属于这种——"→" 通用且比
//            "Right" 更简洁；符号在两个平台都能好看渲染）
//
// 策略（对齐 VS Code / Slack / Notion / GitHub 做法）：
//   - macOS：全用符号（原生习惯 "⌘⇧K"，SF 字体渲染精致）
//   - Windows：默认纯文字（"Ctrl+Tab" 而非 "⌃⇥"），仅方向键等 preferSymbol=true 的键
//     显示符号

// 键名归一化：小写 + 去修饰前缀。event.key 可能是 "ArrowRight" / " " / "Escape"，
// 也可能用户直接传 "arrowright" / "esc"，统一转成 map key。
function normalize(key) {
  const k = String(key || "").trim().toLowerCase();
  // 常见同义词
  const aliases = {
    " ": "space",
    esc: "escape",
    "return": "enter",
    "cmd": "meta",
    "command": "meta",
    "win": "meta",
    "windows": "meta",
    "opt": "alt",
    "option": "alt",
    "del": "delete",
    "ins": "insert",
    "backspace": "backspace",
    "arrowleft": "left",
    "arrowright": "right",
    "arrowup": "up",
    "arrowdown": "down",
  };
  return aliases[k] || k;
}

// key → { text, symbol?, preferSymbol? } 元数据。
const KEY_META = {
  // 动作键：macOS 用符号，Windows 用文字
  tab:       { text: "Tab",       symbol: "⇥" },
  enter:     { text: "Enter",     symbol: "↵" },
  escape:    { text: "Esc",       symbol: "⎋" },
  backspace: { text: "Backspace", symbol: "⌫" },
  delete:    { text: "Del",       symbol: "⌦" },
  space:     { text: "Space",     symbol: "␣" },
  // 方向键：两个平台都用符号（"→" 简洁通用，且这几个符号在 Segoe UI 里字形正常）
  left:      { text: "Left",      symbol: "←", preferSymbol: true },
  right:     { text: "Right",     symbol: "→", preferSymbol: true },
  up:        { text: "Up",        symbol: "↑", preferSymbol: true },
  down:      { text: "Down",      symbol: "↓", preferSymbol: true },
  home:      { text: "Home" },
  end:       { text: "End" },
  pageup:    { text: "PgUp" },
  pagedown:  { text: "PgDn" },
  // 修饰键：Windows 文字，macOS 符号
  ctrl:      { text: "Ctrl",  symbol: "⌃" },
  alt:       { text: "Alt",   symbol: "⌥" },
  shift:     { text: "Shift", symbol: "⇧" },
  meta:      { text: IS_MAC ? "Cmd" : "Win", symbol: IS_MAC ? "⌘" : "⊞" },
  // 左右侧修饰键（Windows 录制单独区分左右侧）：显示简短前缀 + 修饰键名。
  // macOS 概念不区分左右，纯符号即可——但录制结果一般也不会传 RightAlt 到 macOS。
  rightalt:   { text: "R-Alt",   symbol: "⌥" },
  leftalt:    { text: "L-Alt",   symbol: "⌥" },
  rightctrl:  { text: "R-Ctrl",  symbol: "⌃" },
  leftctrl:   { text: "L-Ctrl",  symbol: "⌃" },
  rightshift: { text: "R-Shift", symbol: "⇧" },
  leftshift:  { text: "L-Shift", symbol: "⇧" },
};

/**
 * 单键渲染：返回 `<kbd class="kbd">` 元素。
 *
 * 显示策略：
 *   - macOS：优先符号（有 symbol 就用）
 *   - Windows：优先文字，preferSymbol=true 的键（方向键等）才用符号
 *   - opts.symbolOnly 强制符号模式（若无符号则 fallback 到文字）
 *
 * @param {string} key event.key 或语义名（如 "Tab" / "ArrowRight" / "Ctrl"）
 * @param {{symbolOnly?: boolean}} opts
 * @returns {HTMLElement}
 */
export function renderKey(key, opts = {}) {
  const kbd = document.createElement("kbd");
  kbd.className = "kbd";
  const meta = KEY_META[normalize(key)];

  if (!meta) {
    // 未映射：字母数字/符号原样显示（单字符大写显得干净）
    const s = String(key);
    kbd.textContent = s.length === 1 ? s.toUpperCase() : s;
    return kbd;
  }

  const symbolOnly = opts.symbolOnly ?? false;
  // 何时用符号：macOS / 显式 symbolOnly / 该键标记为 preferSymbol（方向键）
  const useSymbol = meta.symbol && (IS_MAC || symbolOnly || meta.preferSymbol);

  kbd.textContent = useSymbol ? meta.symbol : (meta.text || meta.symbol || String(key));
  return kbd;
}

/**
 * 组合键渲染：返回 `<span class="kbd-group">` 元素，内含多个 `<kbd>` 子元素。
 *
 * 键与键之间用 `+` 连接符（`.kbd-plus`）——让"这几个是同一个组合键"的心智清晰，
 * 避免 `Alt A` 被读成"Alt 和 A 两个独立键"。**设计语言统一**，见
 * docs/production-design/design-language.md §"键盘提示"。
 *
 * @param {string | string[]} combo "Ctrl+K" 或 ["Ctrl", "K"]
 * @param {{symbolOnly?: boolean}} opts 透传给 renderKey
 * @returns {HTMLElement}
 */
export function renderCombo(combo, opts = {}) {
  const group = document.createElement("span");
  group.className = "kbd-group";
  const keys = Array.isArray(combo)
    ? combo
    : String(combo).split(/[+\s]+/).filter(Boolean);
  keys.forEach((k, i) => {
    if (i > 0) {
      const plus = document.createElement("span");
      plus.className = "kbd-plus";
      plus.textContent = "+";
      group.appendChild(plus);
    }
    group.appendChild(renderKey(k, opts));
  });
  return group;
}

// ── 键帽感知的 i18n 模板渲染（0.8.1 优化）─────────────────────────────────
//
// **动机**：statusbar / tooltip 等 UI 有大量含键位的提示文本（"↑↓ 选择"、"Enter 打开"、
// "PgUp/PgDn 翻页"）。原本 i18n 返回字符串直接 textContent，无法插入 kbd DOM。
// 改造为在 i18n 文案里用 `{{key:X}}` 占位符标记键位，渲染时替换成 <kbd class="kbd">。
//
// **模板语法**：
//   `{{key:Tab}}` / `{{key:ArrowUp}}` / `{{key:Ctrl}}` → renderKey(X) 生成 <kbd>
//   `{name}` → params[name]（值可为字符串或 Element；Element 直接 appendChild）
//
// 未识别的 {{...}} 原样保留（不会误吞其他语法）。占位符按顺序渲染。
//
// **用法示例**：
//   renderHint("{{key:Enter}} {label}", { label: "打开" })
//   → DocumentFragment: [<kbd>Enter</kbd>, " ", "打开"]

const HINT_TOKEN = /\{\{key:([^}]+)\}\}|\{([a-zA-Z_][a-zA-Z0-9_]*)\}/g;

/**
 * 把带 `{{key:X}}` / `{name}` 占位符的模板渲染成 DocumentFragment，可直接 appendChild。
 *
 * @param {string} template i18n 文案
 * @param {Record<string, string | Node>} [params] `{name}` 的值——字符串或 Element
 * @returns {DocumentFragment}
 */
export function renderHint(template, params = {}) {
  const frag = document.createDocumentFragment();
  let lastIndex = 0;
  const tpl = String(template ?? "");

  // regex 有全局标志，reset lastIndex 是防御：全局 regex 的 lastIndex 是共享状态，
  // 若同一 regex 对象曾在别处被 exec 提前中断（break/throw），lastIndex 会残留非 0。
  HINT_TOKEN.lastIndex = 0;
  let m;
  while ((m = HINT_TOKEN.exec(tpl)) !== null) {
    // 文本段
    if (m.index > lastIndex) {
      frag.appendChild(document.createTextNode(tpl.slice(lastIndex, m.index)));
    }
    if (m[1] !== undefined) {
      // {{key:X}}
      frag.appendChild(renderKey(m[1]));
    } else {
      // {name}
      const val = params[m[2]];
      if (val instanceof Node) {
        frag.appendChild(val);
      } else if (val !== undefined && val !== null) {
        frag.appendChild(document.createTextNode(String(val)));
      }
      // 未提供的 {name} 视为空（不残留字面 `{foo}`——统一"未定义即空"语义）
    }
    lastIndex = m.index + m[0].length;
  }
  // 尾部剩余文本
  if (lastIndex < tpl.length) {
    frag.appendChild(document.createTextNode(tpl.slice(lastIndex)));
  }
  return frag;
}
