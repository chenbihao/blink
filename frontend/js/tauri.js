//! Tauri 桥接：统一封装 invoke / event.listen 的获取。
//! 兼容 withGlobalTauri 注入的全局对象，屏蔽 TAU.core.invoke ?? TAU.invoke 差异。

const TAU = window.__TAURI__;

/** 调用 Rust command。 */
export const invoke = TAU?.core?.invoke ?? TAU?.invoke;

/** 监听后端事件（返回 unlisten promise）。事件系统不可用时返回 no-op。 */
export function listen(event, handler) {
  if (TAU?.event?.listen) {
    return TAU.event.listen(event, handler);
  }
  return Promise.resolve(() => {});
}
