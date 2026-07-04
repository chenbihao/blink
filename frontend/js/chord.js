//! Chord 增强（0.8.5）：
//! - 增强菜单（§6.1）：主窗可见 + 按住 Alt（body.alt-active）时下拉 Chord 动作提示。
//! - 剪贴板面板（§6.5 Alt+C）：主窗切面板形态显示历史。
//! 悬浮球形态（Alt+Q 划词）是独立 webview（chord-ball.html），不在此模块。

import {
  listChordActions,
  getClipboardHistory,
  recordClipboardHit,
  copyToClipboard,
} from "./api.js";

let chordActions = [];

/** 拉取 Chord 动作列表并渲染（shown 时调一次）。 */
export async function refresh() {
  try {
    chordActions = await listChordActions();
  } catch (e) {
    console.warn("[chord] list_chord_actions 失败", e);
    chordActions = [];
  }
  render();
}

function render() {
  const menu = document.getElementById("chord-menu");
  if (!menu) return;
  if (!chordActions.length) {
    menu.innerHTML = "";
    return;
  }
  menu.innerHTML = chordActions
    .map(
      (a) =>
        `<div class="chord-item" data-key="${a.key}">` +
        `<span class="kbd-group"><kbd>Alt</kbd><kbd>${a.key.toUpperCase()}</kbd></span>` +
        `<span class="chord-label">${escapeHtml(a.label)}</span>` +
        `</div>`
    )
    .join("");
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
  );
}

// ── 剪贴板面板（Alt+C，§6.5 Panel surface）──────────────────────────

/** 显示剪贴板历史面板（拉历史 + 渲染 + 切 mode-clipboard）。 */
export async function showClipboardPanel() {
  let items = [];
  try {
    items = await getClipboardHistory(20);
  } catch (e) {
    console.warn("[chord] get_clipboard_history 失败", e);
  }
  const panel = document.getElementById("clipboard-panel");
  if (!panel) return;
  panel.innerHTML = items.length
    ? items
        .map((it) => {
          const id = escapeHtml(it.id);
          const text = escapeHtml(it.text);
          const preview = escapeHtml(it.preview || it.text);
          return (
            `<div class="clip-item" data-id="${id}" data-text="${text}">` +
            `<div class="clip-preview">${preview}</div>` +
            `</div>`
          );
        })
        .join("")
    : `<div class="clip-empty">无剪贴板历史</div>`;
  document.body.classList.add("mode-clipboard");
  panel.querySelectorAll(".clip-item").forEach((el) => {
    el.addEventListener("click", async () => {
      const t = el.getAttribute("data-text");
      const id = el.getAttribute("data-id");
      try { await copyToClipboard(t); } catch (e) { /* ignore */ }
      try { await recordClipboardHit(id); } catch (e) { /* ignore */ }
      closeClipboardPanel();
    });
  });
}

/** 关闭剪贴板面板（回搜索形态）。 */
export function closeClipboardPanel() {
  document.body.classList.remove("mode-clipboard");
  const panel = document.getElementById("clipboard-panel");
  if (panel) panel.innerHTML = "";
}

export function init() {
  // 菜单数据 lifecycle shown 拉；面板由 chord-panel 事件触发。
}
