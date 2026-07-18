/**
 * Chord 动作 Tab 模块
 * 渲染 chord-actions-container：Alt+<key> 复合动作的启用/禁用列表。
 * 搬自原 settings.js loadChordActions（0.9.5 拆分时遗漏，0.9.5.1 补回）。
 */
import { invoke } from "../../tauri.js";
import { t, onLangChange } from "../../i18n/index.js";
import { saveConfig } from "../../config-keys.js";

/**
 * 初始化 Chord 动作 Tab
 */
export function initChordTab() {
  loadChordActions();

  // 语言切换时重新渲染（toggle 状态已自动保存，重新加载不会丢失）
  onLangChange(loadChordActions);
}

/**
 * 加载并渲染 Chord 动作列表
 */
async function loadChordActions() {
  const container = document.getElementById("chord-actions-container");
  if (!container) return;

  let actions = [];
  try {
    // list_all_chord_actions 返回全部动作（含被禁用的），用于交叉比对展示
    actions = await invoke("list_all_chord_actions");
  } catch (e) {
    console.error("list_all_chord_actions failed:", e);
    return;
  }

  if (!Array.isArray(actions) || actions.length === 0) {
    container.innerHTML = `<div class="action-list-empty">${t("chord.actions.empty")}</div>`;
    return;
  }

  // Chord id → 图标 + 副标题（一眼看懂每个 Chord 做啥）
  const CHORD_META = {
    screenshot: { icon: "🖼", subtitle: t("chord.action.screenshot.subtitle") },
    voice_input: { icon: "🎤", subtitle: t("chord.action.voice_input.subtitle") },
    clipboard_history: { icon: "📋", subtitle: t("chord.action.clipboard_history.subtitle") },
  };

  container.innerHTML = actions
    .map((a) => {
      const meta = CHORD_META[a.id] || { icon: "•", subtitle: "" };
      // key=' '（语音输入）→ 显示 "Space"，与 chord.js / statusbar.js 统一
      const keyLabel = a.key === " " ? "Space" : a.key.toUpperCase();
      const combo = `Alt + ${keyLabel}`;
      const rowClass = a.enabled ? "" : "is-disabled";
      const subtitleHtml = meta.subtitle
        ? `<div class="action-subtitle">${escapeHtml(meta.subtitle)}</div>`
        : "";
      return `<div class="action-list-row ${rowClass}" data-chord-id="${escapeAttr(a.id)}">
        <div class="action-icon">${meta.icon}</div>
        <div class="action-kbd">${combo}</div>
        <div class="action-info">
          <div class="action-title">${escapeHtml(a.label)}</div>
          ${subtitleHtml}
        </div>
        <label class="switch action-toggle">
          <input type="checkbox" class="chord-action-toggle" data-id="${escapeAttr(a.id)}" ${a.enabled ? "checked" : ""} />
          <span class="slider"></span>
        </label>
      </div>`;
    })
    .join("");

  async function save() {
    const disabled = Array.from(
      container.querySelectorAll(".chord-action-toggle"),
    )
      .filter((el) => !el.checked)
      .map((el) => el.dataset.id);
    try {
      await saveConfig("disabled_chord_actions", disabled);
    } catch (e) {
      console.error("set_disabled_chord_actions failed:", e);
    }
  }

  container.querySelectorAll(".chord-action-toggle").forEach((el) => {
    el.addEventListener("change", (e) => {
      const row = e.target.closest(".action-list-row");
      if (row) row.classList.toggle("is-disabled", !e.target.checked);
      save();
    });
  });
}

/** HTML 转义 */
function escapeHtml(str) {
  const div = document.createElement("div");
  div.textContent = str;
  return div.innerHTML;
}

/** 属性转义 */
function escapeAttr(str) {
  return String(str).replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
