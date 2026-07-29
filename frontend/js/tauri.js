//! Tauri 桥接：统一封装 invoke / event.listen / dialog.ask 的获取。
//! 兼容 withGlobalTauri 注入的全局对象，屏蔽 TAU.core.invoke ?? TAU.invoke 差异。

const TAU = window.__TAURI__;

// Tauri 2 + WebView2 下 window.alert/confirm/prompt 是**静默 no-op**——
// 前端代码里 `if (!confirm(...)) return` 会永远走到 return 之后（confirm 返回 undefined → falsy）
// 或永远不走到 return（某些版本返 true → truthy），两种都是隐患。
// 拦一层，任何遗漏调用直接抛错，逼开发者改走 confirmDialog / messageDialog。
// 生产也开——静默失效的 UI 保护比噪音更危险。
["alert", "confirm", "prompt"].forEach((name) => {
  window[name] = function blinkNativeDialogBanned() {
    const msg = `[tauri] window.${name}() 在 Tauri 2 WebView2 下是静默 no-op，请改用 js/tauri.js 里的 confirmDialog / messageDialog。`;
    console.error(msg);
    throw new Error(msg);
  };
});

/** 调用 Rust command。 */
export const invoke = TAU?.core?.invoke ?? TAU?.invoke;

/** 监听后端事件（返回 unlisten promise）。事件系统不可用时返回 no-op。 */
export function listen(event, handler) {
  if (TAU?.event?.listen) {
    return TAU.event.listen(event, handler);
  }
  return Promise.resolve(() => {});
}

// ── 自定义弹窗（替代 Tauri dialog.ask / dialog.message）──────────────────────
// 系统原生弹框样式与应用主题不搭，改为自绘 HTML 弹框，统一视觉风格。
// 复用 .modal-overlay 基础样式 + .confirm-dialog 专属样式。

/** kind → 图标 SVG */
const DIALOG_ICONS = {
  warning: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>`,
  error: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>`,
  info: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>`,
};

/**
 * 创建并显示自定义弹框。
 * @param {{ title?: string, kind?: "info"|"warning"|"error", message: string,
 *           okLabel?: string, cancelLabel?: string|null, dismissable?: boolean }} opts
 * @returns {Promise<boolean>} 用户点击 OK → true，取消 → false
 */
function showCustomDialog(opts) {
  return new Promise((resolve) => {
    const kind = opts.kind || "info";
    const icon = DIALOG_ICONS[kind] || DIALOG_ICONS.info;
    const title = opts.title || (kind === "error" ? "错误" : kind === "warning" ? "警告" : "提示");
    const okLabel = opts.okLabel || "确定";
    const hasCancel = opts.cancelLabel !== null;
    const cancelLabel = opts.cancelLabel || "取消";
    const dismissable = opts.dismissable !== false; // 默认可取消

    const overlay = document.createElement("div");
    overlay.className = "modal-overlay confirm-dialog-overlay";

    const card = document.createElement("div");
    card.className = `confirm-dialog confirm-dialog-${kind}`;

    const iconEl = document.createElement("div");
    iconEl.className = "confirm-dialog-icon";
    iconEl.innerHTML = icon;

    const contentEl = document.createElement("div");
    contentEl.className = "confirm-dialog-content";
    const titleEl = document.createElement("div");
    titleEl.className = "confirm-dialog-title";
    titleEl.textContent = title;
    const msgEl = document.createElement("div");
    msgEl.className = "confirm-dialog-message";
    msgEl.textContent = opts.message;
    contentEl.appendChild(titleEl);
    contentEl.appendChild(msgEl);

    const headerEl = document.createElement("div");
    headerEl.className = "confirm-dialog-header";
    headerEl.appendChild(iconEl);
    headerEl.appendChild(contentEl);

    const actionsEl = document.createElement("div");
    actionsEl.className = "confirm-dialog-actions";

    let resolved = false;
    const finish = (result) => {
      if (resolved) return;
      resolved = true;
      overlay.classList.add("confirm-dialog-closing");
      setTimeout(() => overlay.remove(), 150);
      resolve(result);
    };

    if (hasCancel) {
      const cancelBtn = document.createElement("button");
      cancelBtn.className = "btn btn-small";
      cancelBtn.textContent = cancelLabel;
      cancelBtn.addEventListener("click", () => finish(false));
      actionsEl.appendChild(cancelBtn);
    }

    const okBtn = document.createElement("button");
    okBtn.className = kind === "error" || kind === "warning" ? "btn btn-danger" : "btn-primary";
    okBtn.textContent = okLabel;
    okBtn.addEventListener("click", () => finish(true));
    actionsEl.appendChild(okBtn);

    card.appendChild(headerEl);
    card.appendChild(actionsEl);
    overlay.appendChild(card);

    // 点击 overlay 空白处取消（仅 dismissable 时）
    if (dismissable && hasCancel) {
      overlay.addEventListener("click", (e) => {
        if (e.target === overlay) finish(false);
      });
    }

    // Escape 取消
    const onKey = (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        finish(hasCancel ? false : true);
        document.removeEventListener("keydown", onKey, true);
      } else if (e.key === "Enter") {
        e.preventDefault();
        e.stopPropagation();
        finish(true);
        document.removeEventListener("keydown", onKey, true);
      }
    };
    document.addEventListener("keydown", onKey, true);

    document.body.appendChild(overlay);
    // 聚焦 OK 按钮，方便 Enter 确认
    requestAnimationFrame(() => okBtn.focus());
  });
}

/**
 * 二次确认对话框。
 *
 * 自绘 HTML 弹框，替代 Tauri `dialog.ask()` 系统原生弹框。
 *
 * **签名**：`confirmDialog(message, options?) → Promise<boolean>`
 * - `options.title`：标题；`options.kind`：`"info" | "warning" | "error"`；
 * - `options.okLabel` / `options.cancelLabel`：按钮文案。
 *
 * **兜底**：返回 `false`——**默认拒绝**是危险操作的正确 fallback。
 */
export async function confirmDialog(message, options = {}) {
  try {
    return await showCustomDialog({
      title: options.title,
      kind: options.kind,
      message,
      okLabel: options.okLabel,
      cancelLabel: options.cancelLabel,
    });
  } catch (e) {
    console.error("[tauri] confirmDialog threw:", e);
    return false;
  }
}

/**
 * 单按钮消息对话框（替代 `window.alert`）。
 *
 * 自绘 HTML 弹框，替代 Tauri `dialog.message()` 系统原生弹框。
 *
 * **签名**：`messageDialog(message, options?) → Promise<void>`
 * - `options.title` / `options.kind`（同 confirmDialog）/ `options.okLabel`。
 */
export async function messageDialog(message, options = {}) {
  try {
    await showCustomDialog({
      title: options.title,
      kind: options.kind,
      message,
      okLabel: options.okLabel,
      cancelLabel: null, // 无取消按钮
      dismissable: false,
    });
  } catch (e) {
    console.error("[tauri] messageDialog threw:", e);
  }
}
