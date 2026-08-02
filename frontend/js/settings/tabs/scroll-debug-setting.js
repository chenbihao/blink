//! 长截图诊断开关。使用同源 localStorage，让设置页与截图 overlay 共享状态。

export const SCROLL_DEBUG_STORAGE_KEY = 'blink.scrollDebug';

export function readScrollDebugSetting(storage = window.localStorage) {
  try {
    return storage.getItem(SCROLL_DEBUG_STORAGE_KEY) === '1';
  } catch {
    return false;
  }
}

export function writeScrollDebugSetting(enabled, storage = window.localStorage) {
  if (enabled) storage.setItem(SCROLL_DEBUG_STORAGE_KEY, '1');
  else storage.removeItem(SCROLL_DEBUG_STORAGE_KEY);
}
