//! 主题系统（0.5 完整主题切换）。
//! 根据 AppConfig.theme（auto/light/dark/gruvbox/aquarium/…）应用 data-theme 到 <html>。
//! - dark 是默认态：不设 data-theme，回落 css/theme.css 的 :root（Mocha）。
//! - light 设 [data-theme="light"]（Latte）。
//! - auto 跟随系统：监听 prefers-color-scheme，系统切换时实时生效。
//! - NvChad 系列主题定义在 nvchad-themes.css，新增主题只需在那边加选择器。
//!
//! 主窗口 / 设置页在入口调用 initTheme()；设置页切换主题时调用 applyTheme(mode) 即时预览。

import { invoke } from "./tauri.js";

let mediaQuery = null;
let mediaListener = null;

/**
 * 解析 mode → 实际 data-theme 值。
 * auto 跟随系统（light 媒体查询命中 → "light"，否则 → "dark"）。
 * 其他值（light / dark / gruvbox / …）原样透传。
 */
function resolve(mode) {
  if (mode === "auto") {
    return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
  }
  return mode;
}

/** 把解析后的主题写到 <html>。dark 移除 data-theme（回落 :root），其余设 data-theme。 */
function paint(resolved) {
  const root = document.documentElement;
  if (resolved === "dark") {
    root.removeAttribute("data-theme");
  } else {
    root.setAttribute("data-theme", resolved);
  }
}

/**
 * 应用主题模式。
 * @param {string} mode - auto / light / dark / gruvbox / ...
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

/** 从已读好的 config 对象应用主题（避免重复 invoke get_config）。
 * @param {object} cfg - get_config 返回的 AppConfig 对象 */
export function applyThemeFromConfigData(cfg) {
  applyTheme((cfg && cfg.theme) || "auto");
}

/** 从 AppConfig 读 window_opacity 并设置 CSS 变量 --glass-opacity。启动时与每次 shown/config-changed 刷新。 */
export async function applyGlassOpacityFromConfig() {
  try {
    const cfg = await invoke("get_config");
    if (cfg && cfg.window_opacity !== undefined) {
      document.documentElement.style.setProperty("--glass-opacity", cfg.window_opacity);
    }
  } catch (e) {
    console.error("applyGlassOpacityFromConfig: 读 config 失败", e);
  }
}

/** 从已读好的 config 对象应用毛玻璃透明度（避免重复 invoke get_config）。
 * @param {object} cfg - get_config 返回的 AppConfig 对象 */
export function applyGlassOpacityFromConfigData(cfg) {
  if (cfg && cfg.window_opacity !== undefined) {
    document.documentElement.style.setProperty("--glass-opacity", cfg.window_opacity);
  }
}

