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

/** 触发 Chord 动作（0.8.5 §六）——key 为字母（a/c），后端按 key 分派到 ChordRegistry。
 *  注意：Alt+Space 语音输入不走此路径，由 native hotkey hold 状态机直接处理。 */
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

/** 0.10.7：设置 Chord 独占模式。主窗 Alt hold + chordEligible 时进 true，退出时 false。
 *  后端 LL hook 在 chord mode 下吞掉 chord 键 keydown，独占触发。
 *  0.14：tapKeys 参数让前端直接传入已派生的 tap 键集合，跳过后端 DB 查询。 */
export function setChordMode(on, tapKeys) {
  return invoke("set_chord_mode", { on, tapKeys: tapKeys ?? null });
}

/** 拉取剪贴板历史（Alt+C 面板渲染用）。 */
export function getClipboardHistory(limit) {
  return invoke("get_clipboard_history", { limit: limit ?? 20 });
}

/** 记录剪贴板项命中（点选后调用，频率加权）。 */
export function recordClipboardHit(id) {
  return invoke("record_clipboard_hit", { id });
}

/** 隐藏截图覆盖窗（ESC 取消调；无选区路径走这里）。 */
export function hideScreenshotOverlay() {
  return invoke("hide_screenshot_overlay");
}

/** 0.11.7-f：接收前端合成后的 PNG，写入剪贴板，结束截图会话。 */
export function screenshotCopy(pngData) {
  return invoke("screenshot_copy", { pngData });
}

/**
 * 0.11.7 快路径：跳过前端 PNG 编码 + 后端解码，直接从后端 SESSION 裁剪 BGRA
 * 写剪贴板。仅在无标注 + 有选区时可用；有标注时必须走 `screenshotCopy` 让前端
 * 合成 PNG 传入。坐标是物理像素、SESSION 坐标系。
 */
export function screenshotCopyRegion(x, y, w, h) {
  return invoke("screenshot_copy_region", { x, y, w, h });
}

/** 0.11.7-f：取消截图，结束会话，不保存。 */
export function screenshotCancel() {
  return invoke("screenshot_cancel");
}

/** 0.11.7-f：钉图——接收前端合成后的 PNG + 选区屏幕坐标，创建钉图窗口。 */
export function screenshotPin(pngData, screenX, screenY) {
  return invoke("screenshot_pin", { pngData, screenX, screenY });
}

/** 0.11.8：钉图窗口一次性设置位置+尺寸（物理像素，含 PIN_PAD）。 */
export function screenshotPinTransform(winX, winY, winW, winH) {
  return invoke("screenshot_pin_transform", { winX, winY, winW, winH });
}

/** 0.11.7-f：保存截图选区为文件。path 可选，不传则弹出保存对话框。 */
export function screenshotSave(pngData, path) {
  return invoke("screenshot_save", { pngData, path });
}

/** 0.15.7-R4：把诊断回放文件写入 Blink 日志目录下的受控子目录。 */
export function screenshotSaveReplayFile(directoryName, fileName, data) {
  return invoke("screenshot_save_replay_file", { directoryName, fileName, data });
}

/** 0.11.7-f：通知后端标注模式状态。 */
export function screenshotSetAnnotationMode(active) {
  return invoke("screenshot_set_annotation_mode", { active });
}

/** 0.15.8：列出可吸附窗口（智能窗口吸附用）。返回 PickableWindow 数组。 */
export function screenshotWindowList() {
  return invoke("screenshot_window_list");
}

/** 0.15.7：设置/清除 overlay 捕获排除（WDA_EXCLUDEFROMCAPTURE）。 */
export function screenshotSetCaptureExclusion(exclude) {
  return invoke("screenshot_set_capture_exclusion", { exclude });
}

/** 0.15.7：截取屏幕区域为 RGBA bytes（ArrayBuffer）。 */
export function screenshotCaptureBand(x, y, w, h) {
  return invoke("screenshot_capture_band", { x, y, w, h });
}

/** 0.15.7-R1：截取低分辨率灰度探针，供滚动稳定检测使用。 */
export function screenshotCaptureProbe(x, y, w, h) {
  return invoke("screenshot_capture_probe", { x, y, w, h });
}

/** 0.15.7：转发滚轮事件给目标窗口。 */
export function screenshotForwardWheel(
  hwnd,
  delta,
  screenX,
  screenY,
  passthroughMs = null,
  positionCursor = false,
  forceMessage = false,
) {
  return invoke("screenshot_forward_wheel", {
    hwnd, delta, screenX, screenY, passthroughMs, positionCursor, forceMessage,
  });
}

/** 列出系统已安装的字体名称列表。 */
export function listSystemFonts() {
  return invoke("list_system_fonts");
}

/** 0.11.7-c：OCR 识别图片中的文字，返回 `{text, lines, words, text_angle?}`（0.11.9-b word 级）。 */
export function ocrImage(pngData) {
  return invoke("ocr_image", { pngData });
}

/**
 * 0.11.9-d：翻译文本——直调 translate 插件的 tool（绕过 AI 路径）。
 * @param {string} text 待翻译文本
 * @param {string?} targetLang 目标语言代码(zh/en/ja/ko);省略走插件 setting 默认值
 * @returns {Promise<string>} 译文
 */
export function translateText(text, targetLang) {
  return invoke("translate_text", { text, targetLang: targetLang ?? null });
}

/**
 * 0.11.10-g:批量翻译多行文本。输入 `lines[i]` → 输出 `results[i]`。
 * 单行失败降级到原文（无错误传播），保序。
 */
export function translateLines(lines, targetLang) {
  return invoke("translate_lines", { lines, targetLang: targetLang ?? null });
}

/**
 * **临时**（0.11.7-f 调试用）：把前端消息转发到后端 tracing 控制台。
 * TODO(0.11.7 收尾)：0.11.7 稳定后可移除。
 */
export function frontendLog(level, message) {
  return invoke("frontend_log", { level, message }).catch(() => {});
}
