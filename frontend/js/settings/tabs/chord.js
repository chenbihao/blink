/**
 * Chord 动作 Tab 模块（0.10.7 展开式改造 · 0.10.7.1 视觉与录制链路重写）。
 *
 * 渲染 chord-actions-container：每个 chord 动作一行，可展开进行：
 * - 启用/禁用开关
 * - 键位重绑（点击录制 → 调用后端 record_hotkey 录制 → 校验 Alt+字母 → 保存）
 * - 剪贴板历史动作额外展开详细配置（max_items / retention_days / search_enabled / blacklist）
 *
 * **设计要点**：
 * - 不再用 emoji 图标，靠 kbd 键帽 + 标题/副标题承载信息，与 hotkey tab 视觉一致。
 * - 真 accordion：整行 header 可点击展开（除开关 stopPropagation）；箭头 ▾ 旋转 ▴。
 * - 录制走后端 `record_hotkey` 命令（与 hotkey tab 共用），不再用前端 keydown 监听——
 *   因为 chord 独占模式 hook 会吞掉 Alt+字母 keydown，前端永远收不到事件。
 *   后端录制期间 `is_recording()` 短路在 chord 吞键之前，能正常录到 Alt+字母。
 * - tooltip 直接用 t() 渲染 title 属性（动态生成 HTML 不走 applyI18n）。
 */
import { invoke } from "../../shared/tauri.js";
import { t, onLangChange } from "../../i18n/index.js";
import { saveConfig } from "../../shared/config-keys.js";

/**
 * 初始化 Chord 动作 Tab
 */
export function initChordTab() {
  loadChordActions();

  // 语言切换时重新渲染（toggle 状态已自动保存，重新加载不会丢失）
  onLangChange(loadChordActions);

  // 跨 tab 配置变更通知：其他 tab（如存储页清理调试标记）修改了 screenshot_config
  // 后会 dispatch 此事件，chord tab 需重新加载以刷新开关状态。
  document.addEventListener("blink:config-changed", (e) => {
    const key = e.detail?.key;
    if (key === "screenshot_config" || key === "clipboard_config" || key === "chord_bindings") {
      loadChordActions();
    }
  });
}

/**
 * 加载并渲染 Chord 动作列表（展开式 accordion）。
 */
async function loadChordActions() {
  const container = document.getElementById("chord-actions-container");
  if (!container) return;

  let actions = [];
  try {
    // list_all_chord_actions 返回全部动作（含被禁用的），含 key/semantic/label/surface/enabled
    actions = await invoke("list_all_chord_actions");
  } catch (e) {
    console.error("list_all_chord_actions failed:", e);
    return;
  }

  if (!Array.isArray(actions) || actions.length === 0) {
    container.innerHTML = `<div class="action-list-empty">${t("chord.actions.empty")}</div>`;
    return;
  }

  // 剪贴板详细配置（仅 clipboard_history 动作展开时用）
  let clipboardCfg = null;
  try {
    const fullCfg = await invoke("get_config");
    if (fullCfg?.clipboard) clipboardCfg = fullCfg.clipboard;
  } catch (e) {
    console.warn("load clipboard config failed:", e);
  }

  // 截图详细配置（0.11.10-b：预热 OCR 开关）——仅 screenshot 动作展开时用
  let screenshotCfg = null;
  try {
    const sc = await invoke("get_config_section", { key: "screenshot:config" });
    // 与后端 ScreenshotConfig 的 serde camelCase 对齐;字段缺失走默认
screenshotCfg = {
prewarmOcr: sc?.prewarmOcr !== false,
scrollDebug: sc?.scrollDebug === true,
ocrDebug: sc?.ocrDebug === true,
};
  } catch (e) {
    console.warn("load screenshot config failed:", e);
    screenshotCfg = { prewarmOcr: true, scrollDebug: false, ocrDebug: false };
  }

  // Chord id → 副标题（不再用 emoji 图标，标题/副标题足够承载语义）
  const CHORD_SUBTITLE = {
    chat: t("chord.action.chat.subtitle"),
    screenshot: t("chord.action.screenshot.subtitle"),
    voice_input: t("chord.action.voice_input.subtitle"),
    clipboard_history: t("chord.action.clipboard_history.subtitle"),
    edit: t("chord.action.edit.subtitle"),
    sticky: t("chord.action.sticky.subtitle"),
  };

  container.innerHTML = actions
    .map((a) => renderActionRow(a, CHORD_SUBTITLE[a.id], clipboardCfg, screenshotCfg))
    .join("");

  bindRowEvents(container);
}

/**
 * 渲染单个 chord 动作行（可展开 accordion）。
 */
function renderActionRow(a, subtitle, clipboardCfg, screenshotCfg) {
  subtitle = subtitle || "";
  // key=' '（语音输入）→ 显示 "Space"
  const keyLabel = a.key === " " ? "Space" : a.key.toUpperCase();
  const combo = `Alt + ${keyLabel}`;
  const rowClass = a.enabled ? "" : "is-disabled";

  // voice_input 锁定（键位由 hotkey 配置决定）
  const keyLocked = a.id === "voice_input";
  const lockedMsg = t("chord.binding.voice_input.locked");

  // 整行可点击展开；voice_input 锁定时展开体只有 locked 说明，意义不大但保留以维持一致交互
  const subtitleHtml = subtitle
    ? `<div class="action-subtitle">${escapeHtml(subtitle)}</div>`
    : "";

  // 剪贴板详细配置（仅 clipboard_history 展开）
  const clipboardDetailHtml =
    a.id === "clipboard_history" ? renderClipboardDetail(clipboardCfg) : "";
  // 截图详细配置（仅 screenshot 展开;0.11.10-b 起承载 prewarm_ocr）
  const screenshotDetailHtml =
    a.id === "screenshot" ? renderScreenshotDetail(screenshotCfg) : "";

  // 展开体内容（用 .chord-field 紧凑布局，label 用 --settings-label-width 对齐）
  const bodyInnerHtml = keyLocked
    ? `<div class="chord-locked-note">${lockedMsg}</div>`
    : `<div class="chord-field">
         <label class="setting-label chord-field-label">${t("chord.binding.key.label")}
           <span class="field-hint-icon" title="${escapeAttr(t("chord.binding.key.hint"))}">ⓘ</span>
         </label>
         <div class="chord-field-control">
           <button class="hotkey-btn chord-binding-record" data-id="${escapeAttr(a.id)}" title="${escapeAttr(t("chord.binding.record"))}">
             <span class="chord-binding-combo">${escapeHtml(combo)}</span>
           </button>
           <button class="btn-small chord-binding-reset" data-id="${escapeAttr(a.id)}">${t("chord.binding.reset")}</button>
         </div>
       </div>
       ${clipboardDetailHtml}
       ${screenshotDetailHtml}`;

  return `<div class="action-list-row chord-row ${rowClass}" data-chord-id="${escapeAttr(a.id)}">
    <div class="chord-row-header" data-id="${escapeAttr(a.id)}" role="button" aria-expanded="false" tabindex="0">
      <span class="chord-expand-arrow" aria-hidden="true">▶</span>
      <div class="chord-kbd-slot"><div class="action-kbd">${escapeHtml(combo)}</div></div>
      <div class="action-info">
        <div class="action-title">${escapeHtml(a.label)}</div>
        ${subtitleHtml}
      </div>
      <label class="switch action-toggle" data-id="${escapeAttr(a.id)}">
        <input type="checkbox" class="chord-action-toggle" data-id="${escapeAttr(a.id)}" ${a.enabled ? "checked" : ""} />
        <span class="slider"></span>
      </label>
    </div>
    <div class="chord-row-body" hidden>
      ${bodyInnerHtml}
    </div>
  </div>`;
}

/**
 * 渲染剪贴板详细配置区块（clipboard_history 动作展开体内）。
 *
 * 注意：tooltip 用 t() 直接渲染 title 属性——动态生成 HTML 不走 applyI18n，
 * 故 data-i18n-title 在此处无效，必须用 title=。
 */
function renderClipboardDetail(cfg) {
  cfg = cfg || { display_count: 30, max_items: 200, retention_days: 30, search_enabled: true, blacklist_keywords: [] };
  const blacklist = Array.isArray(cfg.blacklist_keywords)
    ? cfg.blacklist_keywords.join(", ")
    : "";
  return `<div class="chord-clipboard-detail">
    <div class="chord-field">
      <label class="setting-label chord-field-label">${t("chord.clipboard.display_count.label")}
        <span class="field-hint-icon" title="${escapeAttr(t("chord.clipboard.display_count.hint"))}">ⓘ</span>
      </label>
      <input type="number" class="clip-field" data-field="display_count" min="1" max="200" value="${cfg.display_count ?? 30}" />
    </div>
    <div class="chord-field">
      <label class="setting-label chord-field-label">${t("chord.clipboard.max_items.label")}
        <span class="field-hint-icon" title="${escapeAttr(t("chord.clipboard.max_items.hint"))}">ⓘ</span>
      </label>
      <input type="number" class="clip-field" data-field="max_items" min="10" max="5000" value="${cfg.max_items ?? 200}" />
    </div>
    <div class="chord-field">
      <label class="setting-label chord-field-label">${t("chord.clipboard.retention_days.label")}
        <span class="field-hint-icon" title="${escapeAttr(t("chord.clipboard.retention_days.hint"))}">ⓘ</span>
      </label>
      <input type="number" class="clip-field" data-field="retention_days" min="0" max="3650" value="${cfg.retention_days ?? 30}" />
    </div>
    <div class="chord-field">
      <label class="setting-label chord-field-label">${t("chord.clipboard.search_enabled.label")}
        <span class="field-hint-icon" title="${escapeAttr(t("chord.clipboard.search_enabled.hint"))}">ⓘ</span>
      </label>
      <label class="switch switch-sm">
        <input type="checkbox" class="clip-field" data-field="search_enabled" ${cfg.search_enabled !== false ? "checked" : ""} />
        <span class="slider"></span>
      </label>
    </div>
    <div class="chord-field">
      <label class="setting-label chord-field-label">${t("chord.clipboard.blacklist.label")}
        <span class="field-hint-icon" title="${escapeAttr(t("chord.clipboard.blacklist.hint"))}">ⓘ</span>
      </label>
      <input type="text" class="clip-field" data-field="blacklist_keywords" placeholder="${escapeAttr(t("chord.clipboard.blacklist.placeholder"))}" value="${escapeAttr(blacklist)}" />
    </div>
  </div>`;
}

/**
 * 渲染截图详细配置区块（screenshot 动作展开体内，0.11.10-b）。
 *
 * 目前只承载 `prewarm_ocr`（拖完选区就后台跑 OCR,让「识别」/「翻译」秒响应）。
 * 后续 0.11.10-i/j 的背景遮罩策略等也归到此区。
 */
function renderScreenshotDetail(cfg) {
cfg = cfg || { prewarmOcr: true, scrollDebug: false, ocrDebug: false };
return `<div class="chord-screenshot-detail">
<div class="chord-field">
<label class="setting-label chord-field-label">${t("chord.screenshot.prewarm_ocr.label")}
<span class="field-hint-icon" title="${escapeAttr(t("chord.screenshot.prewarm_ocr.hint"))}">ⓘ</span>
</label>
<label class="switch switch-sm">
<input type="checkbox" class="screenshot-field" data-field="prewarm_ocr" ${cfg.prewarmOcr !== false ? "checked" : ""} />
<span class="slider"></span>
</label>
</div>
<div class="chord-field">
<label class="setting-label chord-field-label">${t("chord.screenshot.ocr_debug.label")}
<span class="field-hint-icon" title="${escapeAttr(t("chord.screenshot.ocr_debug.hint"))}">ⓘ</span>
</label>
<label class="switch switch-sm">
<input type="checkbox" class="screenshot-field" data-field="ocr_debug" ${cfg.ocrDebug === true ? "checked" : ""} />
<span class="slider"></span>
</label>
</div>
<div class="chord-field">
<label class="setting-label chord-field-label">${t("chord.screenshot.scroll_debug.label")}
<span class="field-hint-icon" title="${escapeAttr(t("chord.screenshot.scroll_debug.hint"))}">ⓘ</span>
</label>
<label class="switch switch-sm">
<input type="checkbox" class="screenshot-field" data-field="scroll_debug" ${cfg.scrollDebug === true ? "checked" : ""} />
<span class="slider"></span>
</label>
</div>
</div>`;
}

/**
 * 绑定行内事件：展开/收起、启用开关、键位录制、键位重置、剪贴板字段自动保存。
 */
function bindRowEvents(container) {
  // ── 启用/禁用开关 ──
  // 注意：开关在 header 内，必须 stopPropagation 防止点开关也触发展开。
  async function saveDisabled() {
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

  container.querySelectorAll(".action-toggle").forEach((el) => {
    el.addEventListener("click", (e) => e.stopPropagation());
  });

  container.querySelectorAll(".chord-action-toggle").forEach((el) => {
    el.addEventListener("change", (e) => {
      const row = e.target.closest(".action-list-row");
      if (row) row.classList.toggle("is-disabled", !e.target.checked);
      saveDisabled();
    });
  });

  // ── 展开/收起（整行 header 可点击）──
  // 展开状态存在 container DOM 上（container._expandedChordIds），支持多展开
  // （与插件 accordion 一致）。录制/重置后重新渲染可据此恢复展开状态。
  container._expandedChordIds = container._expandedChordIds || new Set();

  function toggleExpand(header) {
    const row = header.closest(".chord-row");
    const body = row?.querySelector(".chord-row-body");
    if (!body) return;
    const chordId = row?.dataset.chordId || "";
    const expanded = body.hasAttribute("hidden");
    if (expanded) {
      body.removeAttribute("hidden");
      header.setAttribute("aria-expanded", "true");
      row?.classList.add("is-expanded");
      container._expandedChordIds.add(chordId);
    } else {
      body.setAttribute("hidden", "");
      header.setAttribute("aria-expanded", "false");
      row?.classList.remove("is-expanded");
      container._expandedChordIds.delete(chordId);
    }
  }

  container.querySelectorAll(".chord-row-header").forEach((header) => {
    // 恢复展开状态：若该 row 之前已展开，重新展开
    const row = header.closest(".chord-row");
    if (row && container._expandedChordIds.has(row.dataset.chordId)) {
      const body = row.querySelector(".chord-row-body");
      if (body && body.hasAttribute("hidden")) {
        toggleExpand(header);
      }
    }

    header.addEventListener("click", () => toggleExpand(header));
    header.addEventListener("keydown", (e) => {
      // Enter / Space 触发展开（键盘可达性）
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        toggleExpand(header);
      }
    });
  });

  // ── 键位录制（复用后端 record_hotkey）──
  container.querySelectorAll(".chord-binding-record").forEach((btn) => {
    btn.addEventListener("click", async (e) => {
      e.stopPropagation();
      await startRecording(btn);
    });
  });

  // ── 键位重置 ──
  container.querySelectorAll(".chord-binding-reset").forEach((btn) => {
    btn.addEventListener("click", async (e) => {
      e.stopPropagation();
      const id = btn.dataset.id;
      // 读当前 chord 配置，把该 id 的 binding.key 清空（触发 default 兜底）
      try {
        const fullCfg = await invoke("get_config");
        const bindings = fullCfg?.chord_bindings;
        if (bindings && bindings[id]) {
          bindings[id].key = "";
          await saveConfig("chord_bindings", bindings);
          // 保持该 row 展开状态，重新渲染后恢复
          container._expandedChordIds.add(id);
          await loadChordActions();
        }
      } catch (err) {
        console.error("reset chord binding failed:", err);
      }
    });
  });

  // ── 剪贴板字段自动保存 ──
  const detail = container.querySelector(".chord-clipboard-detail");
  if (detail) {
    detail.querySelectorAll(".clip-field").forEach((el) => {
      el.addEventListener("change", () => saveClipboardDetail(container));
      // 阻止 input 内点击冒泡到 header 触发展开
      el.addEventListener("click", (e) => e.stopPropagation());
    });
  }

  // ── 截图字段自动保存（0.11.10-b）──
  const shotDetail = container.querySelector(".chord-screenshot-detail");
  if (shotDetail) {
    shotDetail.querySelectorAll(".screenshot-field").forEach((el) => {
      el.addEventListener("change", () => saveScreenshotDetail(container));
      el.addEventListener("click", (e) => e.stopPropagation());
    });
  }
}

/**
 * 键位录制：调用后端 `record_hotkey` 命令录制任意快捷键，前端校验为 Alt+字母后保存。
 *
 * 为何不用前端 keydown：chord 独占模式下，hotkey hook 会吞掉 Alt+字母 keydown
 * （`is_chord_mode() && Alt pressed && is_chord_key()`），前端永远收不到事件。
 * 后端录制期间 `is_recording()` 短路在 chord 吞键逻辑之前，能正常录到 Alt+字母。
 *
 * 校验规则：modifiers 必须包含 Alt（lalt/ralt/alt 任一），key 必须是 a-z 字母。
 * 不符合则提示无效并保持原键不变。
 */
async function startRecording(btn) {
  const id = btn.dataset.id;
  const comboEl = btn.querySelector(".chord-binding-combo");
  if (!comboEl) return;

  const origCombo = comboEl.textContent;
  btn.disabled = true;
  btn.classList.add("recording");
  comboEl.textContent = t("chord.binding.recording");

  try {
    const result = await invoke("record_hotkey");
    // 校验：必须是 Alt（任一侧）+ 字母，不允许其他修饰键
    const hasAlt = Array.isArray(result.modifiers) && result.modifiers.some(
      (m) => m === "alt" || m === "lalt" || m === "ralt",
    );
    const hasOtherMod = Array.isArray(result.modifiers) && result.modifiers.some(
      (m) => m !== "alt" && m !== "lalt" && m !== "ralt",
    );
    const isLetter = typeof result.key === "string" && /^[a-z]$/i.test(result.key);

    if (!hasAlt || hasOtherMod || !isLetter) {
      // 无效组合：提示并恢复原 combo
      flashCombo(comboEl, origCombo, t("chord.binding.invalid"));
      return;
    }

    // 保存新 binding（key 转小写以与 default_key / effective_key 对齐）
    const key = result.key.toLowerCase();
    const fullCfg = await invoke("get_config");
    const bindings = fullCfg?.chord_bindings || {};
    if (!bindings[id]) {
      // 只补 key/modifiers，不写 semantic —— semantic 缺省即走后端 default_semantic()。
      // 早期版本曾写 `semantic: "tap"`，会让 voice_input（若日后允许改键）从 Hold 静默降级 Tap，
      // 并被收进 tap_keys 让 LL hook 吞 Alt+Space。0.11.7 已在后端加 id 特判兜底，此处配合去掉。
      bindings[id] = { key: "", modifiers: ["alt"] };
    }
    bindings[id].key = key;
    await saveConfig("chord_bindings", bindings);
    // 保持该 row 展开状态，重新渲染后恢复
    const container = btn.closest("#chord-actions-container");
    if (container) container._expandedChordIds.add(id);
    await loadChordActions();
  } catch (err) {
    // 录制被取消或超时（后端返回 Err）：恢复原 combo
    console.warn("record chord key failed:", err);
    comboEl.textContent = origCombo;
  } finally {
    btn.disabled = false;
    btn.classList.remove("recording");
  }
}

/**
 * 短暂显示提示文案后恢复原 combo（用于无效录制反馈）。
 */
function flashCombo(comboEl, origCombo, msg) {
  comboEl.textContent = msg;
  setTimeout(() => {
    if (comboEl.textContent === msg) {
      comboEl.textContent = origCombo;
    }
  }, 1500);
}

/**
 * 保存剪贴板详细配置。
 */
async function saveClipboardDetail(container) {
  const detail = container.querySelector(".chord-clipboard-detail");
  if (!detail) return;
  try {
    const fullCfg = await invoke("get_config");
    const clip = fullCfg?.clipboard || {};
    const displayCount = parseInt(detail.querySelector('[data-field="display_count"]')?.value, 10);
    const maxItems = parseInt(detail.querySelector('[data-field="max_items"]')?.value, 10);
    const retentionDays = parseInt(detail.querySelector('[data-field="retention_days"]')?.value, 10);
    const searchEnabled = detail.querySelector('[data-field="search_enabled"]')?.checked !== false;
    const blacklistStr = detail.querySelector('[data-field="blacklist_keywords"]')?.value || "";
    const blacklist = blacklistStr
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);

    const newCfg = {
      enabled: clip.enabled !== false,
      display_count: isNaN(displayCount) ? 30 : displayCount,
      max_items: isNaN(maxItems) ? 200 : maxItems,
      retention_days: isNaN(retentionDays) ? 30 : retentionDays,
      search_enabled: searchEnabled,
      blacklist_keywords: blacklist,
    };
    await saveConfig("clipboard_config", newCfg);
  } catch (e) {
    console.error("save clipboard detail failed:", e);
  }
}

/**
 * 保存截图 detail（0.11.10-b：目前只 prewarm_ocr 一个字段）。
 * 走 set_config('screenshot_config', {...})——后端按 key 路由到 screenshot:config 分片。
 */
async function saveScreenshotDetail(container) {
  const detail = container.querySelector(".chord-screenshot-detail");
  if (!detail) return;
  try {
const prewarmOcr = detail.querySelector('[data-field="prewarm_ocr"]')?.checked !== false;
const scrollDebug = detail.querySelector('[data-field="scroll_debug"]')?.checked === true;
const ocrDebug = detail.querySelector('[data-field="ocr_debug"]')?.checked === true;
await saveConfig("screenshot_config", { prewarmOcr, scrollDebug, ocrDebug });
  } catch (e) {
    console.error("save screenshot detail failed:", e);
  }
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
