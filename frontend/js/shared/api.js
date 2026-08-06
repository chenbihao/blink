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

// 0.17.6: trigger_ai / confirm_ai_action 已删除，主窗口 AI 改走 ChatService。
// 以下 chat_* 命令与对话窗口共用同一套 ChatService。

/** 0.17.6: 发送 AI 对话消息（主窗口 + 对话窗口共用）。
 *  主窗口传 targetWindow="main" + ephemeral=true，使用临时对话记忆。
 *  @param {string} conversationId 对话 ID（主窗口用 crypto.randomUUID() 生成）
 *  @param {string} message 用户消息
 *  @param {object} [opts] 可选参数
 *  @param {string} [opts.groupId] 分组 ID
 *  @param {string} [opts.targetWindow] 目标窗口（默认 "chat"，主窗口传 "main"）
 *  @param {boolean} [opts.ephemeral] 是否临时对话（默认 false）
 *  @returns {Promise<number>} request_id */
export function chatPrompt(conversationId, message, opts = {}) {
  return invoke("chat_prompt", {
    conversationId,
    message,
    groupId: opts.groupId ?? null,
    targetWindow: opts.targetWindow ?? null,
    ephemeral: opts.ephemeral ?? null,
  });
}

/** 0.17.6: 中止 AI 对话请求。
 *  @param {number} requestId
 *  @returns {Promise<boolean>} */
export function chatAbort(requestId) {
  return invoke("chat_abort", { requestId });
}

/** 0.18.0: 清除主窗口 AI 活跃标志（exitAiMode 时调用）。
 *  @returns {Promise<void>} */
export function clearMainAiActive() {
  return invoke("clear_main_ai_active");
}

/** 0.17.6: 确认/拒绝危险操作（主窗口 + 对话窗口共用）。
 *  @param {number} confirmId
 *  @param {boolean} approved
 *  @returns {Promise<boolean>} */
export function confirmChatAction(confirmId, approved) {
  return invoke("confirm_chat_action", { confirmId, approved });
}

/** 0.17.6a: 将主窗口临时对话提升为持久对话。
 *  后端 abort 当前请求 -> 导出临时消息 -> 写入 SQLite -> 清空临时记忆 -> 打开对话窗口。
 *  主窗口前端调用后自行 exitAiMode。
 *  @param {string} conversationId 临时对话 ID
 *  @returns {Promise<void>} */
export function promoteEphemeralConversation(conversationId) {
  return invoke("promote_ephemeral_conversation", { conversationId });
}

/** 0.17.8: 清除所有 AI 权限记忆（ai_permission_memory 表全清）。
 *  @returns {Promise<void>} */
export function clearAllPermissionMemory() {
  return invoke("clear_all_permission_memory");
}

/** 0.17.9: 列出主窗口 AI（Ephemeral）可选的所有 Chat 能力模型。
 *  返回的列表中 is_selected=true 的项为当前选中模型。
 *  @returns {Promise<Array<{id: string, provider_name: string, model_name: string, is_main: boolean, is_light: boolean, is_selected: boolean}>>} */
export function getEphemeralModels() {
  return invoke("get_ephemeral_models");
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

/** 删除指定剪贴板历史记录（右键菜单用）。id 为 clipboard_history 表主键。 */
export function deleteClipboardItem(id) {
  return invoke("delete_clipboard_item", { id });
}

/** 0.17.0：删除指定剪贴板图片条目。imageId 为 clipboard_images 表 id。 */
export function deleteClipboardImage(imageId) {
  return invoke("delete_clipboard_image", { imageId });
}

/** 0.17.0：清空所有剪贴板图片历史。 */
export function clearClipboardImages() {
  return invoke("clear_clipboard_images");
}

/** 0.17.0：手动优化存储（四库 VACUUM）。 */
export function optimizeStorage() {
  return invoke("optimize_storage");
}

/** 触发 Chord 动作（0.8.5 §六）——key 为字母（a/c），后端按 key 分派到 ChordRegistry。
 *  注意：Alt+Space 语音输入不走此路径，由 native hotkey hold 状态机直接处理。
 *  0.16.2：inputText 用于 requires_input=true 的 chord（如 chat），把主窗口输入框文本带入。 */
export function triggerChord(key, inputText, originRef) {
  return invoke("trigger_chord", {
    key,
    inputText: inputText ?? null,
    originRef: originRef ?? null,
  });
}

/** 列出已注册的 Chord 动作（0.8.5 §六 增强菜单渲染用）。每条：{ id, key, label, surface }。 */
export function listChordActions() {
  return invoke("list_chord_actions");
}

/** 当前 Alt 键是否按下（0.8.5 §6.1 前端轮询驱动 alt-active，WebView2 不转发 Alt keydown）。 */
export function isAltDown() {
  return invoke("is_alt_down");
}

/** 0.16.9：获取当前 awareness 选区文本（chord E/S 空闲态上下文解析用）。 */
export function getAwarenessText() {
  return invoke("get_awareness_text");
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

/** 0.18.2：列出前台窗口的 UIA 控件提示（控件级智能吸附用）。返回 ControlHint 数组。 */
export function screenshotControlHints() {
  return invoke("screenshot_control_hints");
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

/** 0.17.5：OCR 诊断——返回已安装语言列表、引擎语言、中文包状态。 */
export function ocrDiagnose() {
  return invoke("ocr_diagnose");
}

/** 通过 Win32 ShellExecuteW 打开 URL / 协议（比 window.open 更可靠，支持 ms-settings: 等）。 */
export function openUrl(url) {
  return invoke("open_url", { url });
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

// ── 0.16.3 内容编辑器 ──

/** 打开内容编辑器窗口。payload 为 EditableContentPayload（camelCase）。 */
export function openContentEditor(payload) {
  return invoke("open_content_editor", { payload });
}

/** 前端 init 时拉取待编辑 payload（取出后清空）。 */
export function getContentEditorPayload() {
  return invoke("get_content_editor_payload");
}

/** 保存编辑后的内容。返回新记录 id（clipboard_new）或原 sticky id（sticky_update）。
 *  0.16.9：savePolicy 为 "clipboard_new"（默认）或 "sticky_update"（回写便签）。 */
export function saveContentEditor(body, originRef, savePolicy) {
  return invoke("save_content_editor", {
    body,
    originRef: originRef ?? null,
    savePolicy: savePolicy ?? null,
  });
}

// ── 0.16.4-0.16.5 剪贴板图片 ──

/** 0.16.4：将剪贴板图片写回系统剪贴板。imageId 为 clipboard_images 表 id。 */
export function copyClipboardImage(imageId) {
  return invoke("copy_clipboard_image", { imageId });
}

/** 0.16.5：将剪贴板图片钉到桌面（pin 窗口）。imageId 为 clipboard_images 表 id。 */
export function pinClipboardImage(imageId) {
  return invoke("pin_clipboard_image", { imageId });
}

// ── 0.16.7-0.16.8 桌面便签 ──

/** 0.16.7：创建便签。返回 StickyNote（含 id）。前端拿到 id 后调 showStickyWindow 打开窗口。 */
export function createStickyNote(content, color) {
  return invoke("create_sticky_note", { content: content ?? "", color: color ?? null });
}

/** 0.16.7：获取单条便签。 */
export function getStickyNote(id) {
  return invoke("get_sticky_note", { id });
}

/** 0.16.7：列出全部便签（按 updated_at 倒序）。 */
export function listStickyNotes() {
  return invoke("list_sticky_notes");
}

/** 0.16.7：更新便签内容（前端防抖 500ms 后调用）。 */
export function updateStickyContent(id, content) {
  return invoke("update_sticky_content", { id, content });
}

/** 0.16.7：更新便签外观（颜色 + 可选格式）。 */
export function updateStickyAppearance(id, color, format) {
  return invoke("update_sticky_appearance", { id, color, format: format ?? null });
}

/** 0.16.7：更新便签窗口几何（位置 + 尺寸，物理像素）。 */
export function updateStickyGeometry(id, x, y, width, height) {
  return invoke("update_sticky_geometry", { id, x, y, width, height });
}

/** 0.16.7：设置便签可见性（关闭 = 隐藏）。 */
export function setStickyVisible(id, visible) {
  return invoke("set_sticky_visible", { id, visible });
}

/** 0.16.7：设置便签置顶。 */
export function setStickyAlwaysOnTop(id, alwaysOnTop) {
  return invoke("set_sticky_always_on_top", { id, alwaysOnTop });
}

/** 0.16.7：删除便签（永久）。 */
export function deleteStickyNote(id) {
  return invoke("delete_sticky_note", { id });
}

/** 0.16.7：获取便签统计 { count, visible }。 */
export function getStickyStats() {
  return invoke("get_sticky_stats");
}

/** 0.16.8：显示便签窗口（后端创建/复用 Tauri 窗口）。
 *  0.18.4：可选 atCursor=true 时，新便签定位到鼠标处（标题栏中心对准光标）。
 */
export function showStickyWindow(stickyId, atCursor = false) {
  return invoke("show_sticky_window_cmd", { stickyId, atCursor });
}

/** 0.16.8：销毁便签窗口（删除数据后调用）。 */
export function destroyStickyWindow(stickyId) {
  return invoke("destroy_sticky_window_cmd", { stickyId });
}

/** 0.16.10：显示便签管理窗口。 */
export function showStickyManager() {
  return invoke("show_sticky_manager_cmd");
}

// ── 0.17.7 便签回收站 ──────────────────────────────────

/** 0.17.7：将便签移入回收站（软删除）。 */
export function trashStickyNote(id) {
  return invoke("trash_sticky_note", { id });
}

/** 0.17.7：从回收站恢复便签。 */
export function restoreStickyNote(id) {
  return invoke("restore_sticky_note", { id });
}

/** 0.17.7：列出回收站中的便签。 */
export function listTrashedStickyNotes() {
  return invoke("list_trashed_sticky_notes");
}

/** 0.17.7：清空回收站。返回删除的行数。 */
export function clearTrashedStickyNotes() {
  return invoke("clear_trashed_sticky_notes");
}
