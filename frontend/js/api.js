//! 后端命令封装：把 Rust command 名/参数收敛到一处，业务模块只调语义化函数。
//! 新增 command 时只改这里，调用方无需关心字符串与参数名。

import { invoke } from "./tauri.js";

export { invoke };

/** 搜索应用（含计算结果），返回 AppEntry 数组。seq 为请求序号（后端 async 增量回带，前端校验）。 */
export function searchApps(query, seq) {
  return invoke("search_apps", { query, seq });
}

/** 启动应用（打开 lnk）。 */
export function launchApp(lnkPath) {
  return invoke("launch_app", { lnkPath });
}

/** 运行内置动作（0.8.0 §1.3）——id 为动作注册表 key，arg 为可选参数。 */
export function runBuiltinAction(id, arg) {
  return invoke("run_builtin_action", { id, arg: arg ?? null });
}

/** AI Dangerous 动作确认执行（0.9.2 第二步）——用户在确认卡片上按 Enter 后调用。 */
export function confirmAiAction(actionName, arguments_) {
  return invoke("confirm_ai_action", { action_name: actionName, arguments: arguments_ });
}

/** 隐藏主窗口。 */
export function hideWindow() {
  return invoke("hide_window");
}

/** 调整主窗口大小（弹性窗口）。 */
export function resizeWindow(width, height) {
  return invoke("resize_window", { width, height });
}

/** 打开文件/快捷方式所在文件夹（explorer 定位选中）。 */
export function openContainingFolder(path) {
  return invoke("open_containing_folder", { path });
}

/** 解析 .lnk 快捷方式目标，用 explorer /select 定位到目标文件。 */
export function openLnkTarget(lnkPath) {
  return invoke("open_lnk_target", { lnkPath });
}

/** 重置某项的历史记录权重（右键菜单用）。 */
export function resetItemHistory(lnkPath) {
  return invoke("reset_item_history", { lnkPath });
}

/** 将文本写入系统剪贴板（后端 Windows API，独立窗口中也可靠）。 */
export function copyToClipboard(text) {
  return invoke("copy_to_clipboard", { text });
}

/** 触发 Chord 动作（0.8.5 §六）——key 为字母（a/q/c），后端按 key 分派到 ChordRegistry。 */
export function triggerChord(key) {
  return invoke("trigger_chord", { key });
}

/** 列出已注册的 Chord 动作（0.8.5 §六 增强菜单渲染用）。每条：{ id, key, label, surface }。 */
export function listChordActions() {
  return invoke("list_chord_actions");
}

/** 当前 Alt 键是否按下（0.8.5 §6.1 前端轮询驱动 alt-active，WebView2 不转发 Alt keydown）。 */
export function isAltDown() {
  return invoke("is_alt_down");
}

/** 隐藏 chord-ball 悬浮窗（悬浮球内点击/ESC 调）。 */
export function hideChordBall() {
  return invoke("hide_chord_ball");
}

/** 确认划词：读选区缓存 + 主窗显示翻译结果 + 隐藏球。 */
export function confirmChordSelection() {
  return invoke("confirm_chord_selection");
}

/** 轮询选区缓存（chord-ball 前端用，检测划词是否成功）。返回文本或 null。 */
export function pollChordSelection() {
  return invoke("poll_chord_selection");
}

/** 拉取剪贴板历史（Alt+C 面板渲染用）。 */
export function getClipboardHistory(limit) {
  return invoke("get_clipboard_history", { limit: limit ?? 20 });
}

/** 记录剪贴板项命中（点选后调用，频率加权）。 */
export function recordClipboardHit(id) {
  return invoke("record_clipboard_hit", { id });
}

/** 确认截图选区（0.8.7）：只传物理像素坐标，裁剪由后端从 SESSION 完成。 */
export function captureRegion(x, y, w, h) {
  return invoke("capture_region", { x, y, w, h });
}

/** 隐藏截图覆盖窗（ESC 取消调）。 */
export function hideScreenshotOverlay() {
  return invoke("hide_screenshot_overlay");
}
