//! 主题系统（0.5 完整主题切换）。
//! 根据 AppConfig.theme（auto/light/dark）应用 data-theme 到 <html>。
//! - dark 是默认态：不设 data-theme，回落 theme.css 的 :root（Mocha）。
//! - light 设 [data-theme="light"]（Latte）。
//! - auto 跟随系统：监听 prefers-color-scheme，系统切换时实时生效。
//!
//! 主窗口 / 设置页在入口调用 initTheme()；设置页切换主题时调用 applyTheme(mode) 即时预览。

import { invoke } from "./tauri.js";

let mediaQuery = null;
let mediaListener = null;

/** 解析 auto/light/dark → 实际应渲染 'light' | 'dark'。 */
function resolve(mode) {
  if (mode === "light") return "light";
  if (mode === "dark") return "dark";
  // auto：跟随系统（light 媒体查询命中 → light）
  return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

/** 把解析后的主题写到 <html>。dark 移除 data-theme（回落 :root），light 设 data-theme="light"。 */
function paint(resolved) {
  const root = document.documentElement;
  if (resolved === "light") {
    root.setAttribute("data-theme", "light");
  } else {
    root.removeAttribute("data-theme");
  }
}

/**
 * 应用主题模式。
 * @param {string} mode - auto / light / dark
 */
export function applyTheme(mode) {
  paint(resolve(mode));
  if (mode === "auto") {
    ensureMediaListener();
  } else {
    removeMediaListener();
  }
}

/** auto 模式：挂系统主题监听，系统切换时实时重绘。 */
function ensureMediaListener() {
  if (mediaListener) return;
  mediaQuery = window.matchMedia("(prefers-color-scheme: light)");
  mediaListener = (e) => paint(e.matches ? "light" : "dark");
  mediaQuery.addEventListener("change", mediaListener);
}

/** 非 auto 模式：移除监听，避免系统切换干扰用户显式选择。 */
function removeMediaListener() {
  if (mediaQuery && mediaListener) {
    mediaQuery.removeEventListener("change", mediaListener);
  }
  mediaQuery = null;
  mediaListener = null;
}

/** 从 AppConfig 读 theme 并应用。启动时与每次窗口 shown 刷新时调用。读失败回退 auto。 */
export async function applyThemeFromConfig() {
  let mode = "auto";
  try {
    const cfg = await invoke("get_config");
    if (cfg && cfg.theme) mode = cfg.theme;
  } catch (e) {
    console.error("applyThemeFromConfig: 读 config 失败，回退 auto", e);
  }
  applyTheme(mode);
}
