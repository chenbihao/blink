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

/**
 * 二次确认对话框。
 *
 * **背景**：Tauri 2 + WebView2 下 `window.confirm()` 不会弹框、直接返回 truthy，
 * 导致「危险操作 confirm 后」的逻辑形同虚设（点 ✕ 秒删）。统一走 plugin-dialog
 * 的 `ask()` 才是真弹框。
 *
 * **签名**：`ask(message, options?) → Promise<boolean>`
 * - `options.title`：标题；`options.kind`：`"info" | "warning" | "error"`；
 * - `options.okLabel` / `options.cancelLabel`：按钮文案。
 *
 * **兜底**：dialog plugin 不可用时（capability 缺失或环境异常），返回 `false`
 * ——**默认拒绝**是危险操作的正确 fallback，宁可让用户重试也不误删。
 */
export async function confirmDialog(message, options = {}) {
  const ask = TAU?.dialog?.ask;
  if (typeof ask !== "function") {
    console.error("[tauri] dialog.ask unavailable, refusing dangerous action:", message);
    return false;
  }
  try {
    return await ask(message, options);
  } catch (e) {
    console.error("[tauri] dialog.ask threw:", e);
    return false;
  }
}

/**
 * 单按钮消息对话框（替代 `window.alert`）。
 *
 * **背景**：与 [[confirmDialog]] 同因——Tauri 2 + WebView2 下 `window.alert()`
 * 不弹框，用户看不到「保存失败」等错误提示，只能翻控制台。
 *
 * **签名**：`message(message, options?) → Promise<void>`
 * - `options.title` / `options.kind`（同 ask）/ `options.okLabel`。
 *
 * **兜底**：dialog plugin 不可用时走 console.error，不阻塞流程。
 */
export async function messageDialog(message, options = {}) {
  const msg = TAU?.dialog?.message;
  if (typeof msg !== "function") {
    console.error("[tauri] dialog.message unavailable:", message);
    return;
  }
  try {
    await msg(message, options);
  } catch (e) {
    console.error("[tauri] dialog.message threw:", e);
  }
}
