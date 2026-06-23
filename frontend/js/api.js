//! 后端命令封装：把 Rust command 名/参数收敛到一处，业务模块只调语义化函数。
//! 新增 command 时只改这里，调用方无需关心字符串与参数名。

import { invoke } from "./tauri.js";

/** 搜索应用（含计算结果），返回 AppEntry 数组。 */
export function searchApps(query) {
  return invoke("search_apps", { query });
}

/** 启动应用（打开 lnk）。 */
export function launchApp(lnkPath) {
  return invoke("launch_app", { lnkPath });
}

/** 隐藏主窗口。 */
export function hideWindow() {
  return invoke("hide_window");
}

/** 调整主窗口大小（弹性窗口）。 */
export function resizeWindow(width, height) {
  return invoke("resize_window", { width, height });
}
