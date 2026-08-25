//! 后端命令封装：把 Rust command 名/参数收敛到一处，业务模块只调语义化函数。
//! 新增 command 时只改这里，调用方无需关心字符串与参数名。

import {invoke} from "./tauri.js";

export {invoke};

/** 搜索应用（含计算结果），返回 AppEntry 数组。seq 为请求序号（后端 async 增量回带，前端校验）。 */
export function searchApps(query, seq) {
    return invoke("search_apps", {query, seq});
}

/** 剪贴板模式直接搜索（bypass SearchService pipeline）。
 *  Alt+C 进入剪贴板模式后，前端输入直接调此命令，
 *  不经过 SearchService::search / IntentRouter / get_weights / Route 分派。
 *  @param {string} query 搜索词（用户原始输入，无需"剪贴板"前缀）
 *  @param {number} seq 请求序号
 *  @returns {Promise<{entries: Array, suggestion: *}>} */
export function searchClipboard(query, seq) {
    return invoke("search_clipboard", {query, seq});
}

/** 启动应用（打开 lnk）。 */
export function launchApp(lnkPath) {
    return invoke("launch_app", {lnkPath});
}

/** 运行内置动作（0.8.0 §1.3）——id 为动作注册表 key，arg 为可选参数。 */
export function runBuiltinAction(id, arg) {
    return invoke("run_builtin_action", {id, arg: arg ?? null});
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
 *  @param {boolean} [opts.thinkingEnabled] 是否启用深度思考（默认 false）
 *  @param {string|null} [opts.reasoningEffort] 思考强度线值（0.21.17；null = auto/不发送）
 *  @returns {Promise<number>} request_id */
export function chatPrompt(conversationId, message, opts = {}) {
    return invoke("chat_prompt", {
        conversationId,
        message,
        groupId: opts.groupId ?? null,
        targetWindow: opts.targetWindow ?? null,
        ephemeral: opts.ephemeral ?? null,
        thinkingEnabled: opts.thinkingEnabled ?? false,
        reasoningEffort: opts.reasoningEffort ?? null,
    });
}

/** 0.17.6: 中止 AI 对话请求。
 *  @param {number} requestId
 *  @returns {Promise<boolean>} */
export function chatAbort(requestId) {
    return invoke("chat_abort", {requestId});
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
    return invoke("confirm_chat_action", {confirmId, approved});
}

/** 0.17.6a: 将主窗口临时对话提升为持久对话。
 *  后端 abort 当前请求 -> 导出临时消息 -> 写入 SQLite -> 清空临时记忆 -> 打开对话窗口。
 *  主窗口前端调用后自行 exitAiMode。
 *  @param {string} conversationId 临时对话 ID
 *  @returns {Promise<void>} */
export function promoteEphemeralConversation(conversationId) {
    return invoke("promote_ephemeral_conversation", {conversationId});
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
    return invoke("resize_window", {width, height});
}

/** 打开文件/快捷方式所在文件夹（explorer 定位选中）。 */
export function openContainingFolder(path) {
    return invoke("open_containing_folder", {path});
}

/** 解析 .lnk 快捷方式目标，用 explorer /select 定位到目标文件。 */
export function openLnkTarget(lnkPath) {
    return invoke("open_lnk_target", {lnkPath});
}

/** 重置某项的历史记录权重（右键菜单用）。 */
export function resetItemHistory(lnkPath) {
    return invoke("reset_item_history", {lnkPath});
}

/** 将文本写入系统剪贴板（后端 Windows API，独立窗口中也可靠）。 */
export function copyToClipboard(text) {
    return invoke("copy_to_clipboard", {text});
}

/** 0.21.x：剪贴板「上屏」——后端把文本注入唤起前的前台应用输入框。
 *  目标非输入框时后端回退为复制（文本写入系统剪贴板）。后端负责隐藏主窗口。 */
export function pasteToInput(text) {
    return invoke("paste_to_input", {text});
}

/** 删除指定剪贴板历史记录（右键菜单用）。id 为 clipboard_history 表主键。 */
export function deleteClipboardItem(id) {
    return invoke("delete_clipboard_item", {id});
}

/** 0.17.0：删除指定剪贴板图片条目。imageId 为 clipboard_images 表 id。 */
export function deleteClipboardImage(imageId) {
    return invoke("delete_clipboard_image", {imageId});
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

/** 0.16.9：获取当前 awareness 选区文本（chord E/S 空闲态上下文解析用）。 */
export function getAwarenessText() {
    return invoke("get_awareness_text");
}

// ── 输入状态 ──

/**
 * 注册主窗口输入视图，返回初始快照 + view_epoch。
 * 前端先注册 INPUT_STATE_CHANGED listener，再调用此 command。
 * @param {boolean} queryEmpty 当前搜索框是否为空
 * @param {boolean} aiMode 当前是否处于 AI 模式
 * @param {boolean} clipboardMode 当前是否处于剪贴板模式（0.20.8 新增）
 * @returns {Promise<{viewEpoch: number, state: {revision: number, altDown: boolean, windowVisible: boolean, exclusiveChordActive: boolean}}>}
 */
export function registerMainInputView(queryEmpty, aiMode, clipboardMode) {
    return invoke("register_main_input_view", {queryEmpty, aiMode, clipboardMode});
}

/**
 * 更新主窗口输入视图上下文（query 空/非空、AI mode 变化时调）。
 * 只在离散状态变化时调用，不逐字符发送。
 * @param {number} viewEpoch register_main_input_view 返回的 view_epoch
 * @param {number} revision 递增的 context revision
 * @param {boolean} queryEmpty
 * @param {boolean} aiMode
 * @param {boolean} clipboardMode 剪贴板模式是否活跃（0.20.8 新增）
 */
export function updateMainInputContext(viewEpoch, revision, queryEmpty, aiMode, clipboardMode) {
    return invoke("update_main_input_context", {viewEpoch, revision, queryEmpty, aiMode, clipboardMode});
}

/** 拉取剪贴板历史（Alt+C 面板渲染用）。 */
export function getClipboardHistory(limit) {
    return invoke("get_clipboard_history", {limit: limit ?? 20});
}

/** 记录剪贴板项命中（点选后调用，频率加权）。 */
export function recordClipboardHit(id) {
    return invoke("record_clipboard_hit", {id});
}

/** 按 id 拉取完整 text（延迟加载）。
 *  搜索路径只携带 id + preview（80 字符截断），用户选中某条历史时
 *  调此函数按需拉取完整 text。避免搜索路径预载 500 条完整 text 导致 MB 级 JSON。
 *  @param {string} id clipboard_history 表主键
 *  @returns {Promise<string|null>} 完整文本（未找到返回 null） */
export function getClipboardText(id) {
    return invoke("get_clipboard_text", {id});
}

/** 0.20.2：按 id 批量拉取完整 text（批量原子复制用）。
 *  @param {string[]} ids clipboard_history 表主键列表
 *  @returns {Promise<Array<{id: string, text: string|null}>>} 批量结果，顺序与输入一致 */
export function getClipboardTextBatch(ids) {
    return invoke("get_clipboard_text_batch", {ids});
}

/** 隐藏截图覆盖窗（ESC 取消调；无选区路径走这里）。 */
export function hideScreenshotOverlay() {
    return invoke("hide_screenshot_overlay");
}

/** 0.11.7-f：接收前端合成后的 PNG，写入剪贴板，结束截图会话。
 *  0.19.14 raw IPC：直接传 Uint8Array，避开 JSON 序列化大 PNG 的开销。 */
export function screenshotCopy(pngData) {
    return invoke("screenshot_copy", pngData);
}

/**
 * 0.11.7 快路径：跳过前端 PNG 编码 + 后端解码，直接从后端 SESSION 裁剪 BGRA
 * 写剪贴板。仅在无标注 + 有选区时可用；有标注时必须走 `screenshotCopy` 让前端
 * 合成 PNG 传入。坐标是物理像素、SESSION 坐标系。
 */
export function screenshotCopyRegion(x, y, w, h) {
    return invoke("screenshot_copy_region", {x, y, w, h});
}

/** P7：有标注 copy 直传 raw RGBA → 后端 swap 为 BGRA → 写剪贴板。
 *  消除前端 toBlob('image/png')（~160ms）+ 后端 PNG decode（~289ms）。
 *  raw IPC：RGBA 直接传 Uint8Array，w/h 走 headers。 */
export function screenshotCopyRgba(rgbaData, w, h) {
    return invoke("screenshot_copy_rgba", rgbaData, {
        headers: {
            "w": String(w),
            "h": String(h),
        },
    });
}

/** 0.11.7-f：取消截图，结束会话，不保存。 */
export function screenshotCancel() {
    return invoke("screenshot_cancel");
}

/** 0.11.7-f：钉图——接收前端合成后的 PNG + 选区屏幕坐标，创建钉图窗口。
 *  0.18.3：showTranslating=true 时在 pin 窗口中心显示「翻译中」指示器。
 *  0.19.14 raw IPC：PNG 直接传 Uint8Array，其余参数走 headers。 */
export function screenshotPin(pngData, screenX, screenY, showTranslating) {
    return invoke("screenshot_pin", pngData, {
        headers: {
            "screen-x": String(screenX),
            "screen-y": String(screenY),
            "show-translating": String(showTranslating ?? false),
        },
    });
}

/** 0.19.14 Pin 快路径：无标注时后端直接从 SESSION 裁剪 BGRA → 编码 PNG → show_pin。
 *  跳过前端 toBlob + IPC PNG 往返，全屏 pin 从 ~1280ms → ~40ms。 */
export function screenshotPinRegion(x, y, w, h, screenX, screenY) {
    return invoke("screenshot_pin_region", {x, y, w, h, screenX, screenY});
}

/** 0.18.3：原地刷新钉图窗口的图片（不重定位、不重置缩放）。
 *  showTranslating=false 时同时隐藏「翻译中」指示器。
 *  0.19.14 raw IPC：PNG 直接传 Uint8Array，showTranslating 走 headers。 */
export function screenshotPinRefresh(pngData, showTranslating) {
    return invoke("screenshot_pin_refresh", pngData, {
        headers: {"show-translating": String(showTranslating ?? false)},
    });
}

/** 0.11.8：钉图窗口一次性设置位置+尺寸（物理像素，含 PIN_PAD）。
 *  多 Pin：label 为当前窗口 label。 */
export function screenshotPinTransform(label, winX, winY, winW, winH) {
    return invoke("screenshot_pin_transform", {label, winX, winY, winW, winH});
}

/** 0.19.4：钉图窗口仅移动位置（拖拽专用+尺寸专用，零 resize 开销）。
 *  多 Pin：label 为当前窗口 label。 */
export function screenshotPinMove(label, winX, winY) {
    return invoke("screenshot_pin_move", {label, winX, winY});
}

/** 获取 Pin 窗口的当前物理矩形和目标屏 DPR。
 *  用于 DPI reconcile：onScaleChanged 或拖动跨 DPI 边界后回读窗口实际物理位置。
 *  @param {string} label 窗口 label
 *  @returns {Promise<{x: number, y: number, w: number, h: number, dpr: number} | null>} */
export function screenshotPinGetRect(label) {
    return invoke("screenshot_pin_get_rect", {label});
}

/** 0.11.7-f：保存截图选区为文件。path 可选，不传则弹出保存对话框。
 *  0.19.14 raw IPC：PNG 直接传 Uint8Array，path 走 headers。 */
export function screenshotSave(pngData, path) {
    const headers = {};
    if (path) headers["path"] = path;
    return invoke("screenshot_save", pngData, {headers});
}

/** 0.19.14 raw IPC：PNG 直接传 Uint8Array，避开 JSON 序列化开销。 */
export function imageEditorCopy(pngData) {
    return invoke('image_editor_copy', pngData);
}

export function imageEditorPin(pngData, showTranslating) {
    return invoke('image_editor_pin', pngData, {
        headers: {'show-translating': String(showTranslating ?? false)},
    });
}

export function imageEditorSave(pngData, path) {
    const headers = {};
    if (path) headers['path'] = path;
    return invoke('image_editor_save', pngData, {headers});
}

/** 0.20.x：把编辑结果应用回原 pin 窗口（pin 来源编辑器的勾按钮）。
 *  @param {string} label 原 pin 窗口 label；原窗口已关时后端自动重新 pin 到桌面。
 *  raw IPC：PNG 直接传 Uint8Array，label 走 headers。 */
export function imageEditorApplyToPin(pngData, label) {
    const headers = {};
    if (label) headers['label'] = label;
    return invoke('image_editor_apply_to_pin', pngData, {headers});
}

export function imageEditorCancel() {
    return invoke('image_editor_cancel');
}

// ── 0.20.4：多来源图片编辑入口 ──

/** 0.20.4：从当前系统剪贴板图片打开编辑器。
 *  剪贴板无图片时 reject { code: "invalid_state", detail: { reason: "clipboard_no_image" } }。
 *  编辑会话已活跃时 reject { code: "invalid_state", detail: { reason: "already_active" } }
 *  （同时后端激活现有编辑器窗口）。用 normalizeError 归一化。 */
export function openImageEditorFromClipboard() {
    return invoke('open_image_editor_from_clipboard');
}

/** 0.20.4：从剪贴板历史图片打开编辑器。
 *  @param {string} imageId clipboard_images 表 id
 *  不先覆盖系统剪贴板。 */
export function openImageEditorFromHistory(imageId) {
    return invoke('open_image_editor_from_history', {imageId});
}

/** 0.20.4：从仍显示的 pin 窗口打开编辑器。
 *  @param {string} windowLabel pin 窗口 label
 *  不先覆盖系统剪贴板。pin 图窗口本身不受影响。 */
export function openImageEditorFromPin(windowLabel) {
    return invoke('open_image_editor_from_pin', {windowLabel});
}

/** 多 Pin N+1：前端 preheat init 完成后调用，将 spare 注册为可用。 */
export function pinSpareReady() {
    return invoke("pin_spare_ready");
}

/** 将 pin 窗口图片复制到剪贴板。0.19.14 raw IPC。 */
export function pinSaveClipboard(pngData) {
    return invoke("pin_save_clipboard", pngData);
}

/** 将 pin 窗口图片另存为文件，同时复制到剪贴板。path 可选，不传则弹出保存对话框。0.19.14 raw IPC。 */
export function pinSaveAs(pngData, path) {
    const headers = {};
    if (path) headers["path"] = path;
    return invoke("pin_save_as", pngData, {headers});
}

/** 0.15.7-R4：把诊断回放文件写入 Blink 日志目录下的受控子目录。 */
export function screenshotSaveReplayFile(directoryName, fileName, data) {
    return invoke("screenshot_save_replay_file", {directoryName, fileName, data});
}

/** 0.11.7-f：通知后端标注模式状态。 */
export function screenshotSetAnnotationMode(active) {
    return invoke("screenshot_set_annotation_mode", {active});
}

/** 0.15.8：列出可吸附窗口（智能窗口吸附用）。返回 PickableWindow 数组。 */
export function screenshotWindowList() {
    return invoke("screenshot_window_list");
}

/** 0.18.x：流式收集控件 hints（每层 emit 一批，结束发 done）。
 *  hwnd 为目标窗口的 HWND（isize），后端校验后用于 UIA 遍历。 */
export function screenshotControlHints(hwnd, generation) {
    return invoke("screenshot_control_hints", {hwnd, generation});
}

/** 0.15.7：设置/清除 overlay 捕获排除（WDA_EXCLUDEFROMCAPTURE）。 */
export function screenshotSetCaptureExclusion(exclude) {
    return invoke("screenshot_set_capture_exclusion", {exclude});
}

/** 0.15.7：截取屏幕区域为 RGBA bytes（ArrayBuffer）。 */
export function screenshotCaptureBand(x, y, w, h) {
    return invoke("screenshot_capture_band", {x, y, w, h});
}

/** 0.15.7-R1：截取低分辨率灰度探针，供滚动稳定检测使用。 */
export function screenshotCaptureProbe(x, y, w, h) {
    return invoke("screenshot_capture_probe", {x, y, w, h});
}

/** 返回当前光标的物理屏幕坐标（虚拟屏幕坐标系），供取色器直接采样。 */
export function screenshotCursorPosition() {
    return invoke("screenshot_cursor_position");
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

/** 0.11.7-c：OCR 识别图片中的文字，返回 `{text, lines, words, text_angle?}`（0.11.9-b word 级）。
*  0.19.14 raw IPC：PNG 直接传 Uint8Array。
*  0.22.4：可选 screenshotSession/screenshotRevision 参数，携带截图来源信息。
*  0.22.4 Task 6：同步产生 requestId（UUID），立即暴露 cancel()。
*  Task 11：cancel 幂等——多次调用安全，只发一次后端取消请求。
*
*  @returns {{ requestId: string, promise: Promise<{result: *, requestId: string}>, cancel: () => Promise<boolean> }} */
export function ocrImage(pngData, screenshotSession, screenshotRevision) {
    const headers = {};
    if (screenshotSession != null && screenshotRevision != null) {
        headers['X-Screenshot-Session'] = String(screenshotSession);
        headers['X-Screenshot-Revision'] = String(screenshotRevision);
    }
    // Task 6: 使用 crypto.randomUUID()，不用 4 hex 随机尾缀
    const requestId = (typeof crypto !== 'undefined' && crypto.randomUUID)
        ? crypto.randomUUID()
        : `ocr-${Date.now()}-${Math.random().toString(16).slice(2, 10)}`;
    headers['X-Request-Id'] = requestId;

    let cancelled = false;
    const promise = invoke("ocr_image", pngData, {headers}).then(result => ({result, requestId}));

    /** 立即可调用的 cancel——发送取消请求到后端。
     *  Task 11: 幂等——多次调用只发一次后端取消请求，返回同一个 Promise。 */
    const cancel = () => {
        if (cancelled) return Promise.resolve(false);
        cancelled = true;
        return cancelOcrRequest(requestId);
    };

    return {requestId, promise, cancel};
}

/** 0.22.4：取消在途的 OCR 请求。
*  @param {string} requestId — ocrImage 返回的 requestId
*  @returns {Promise<boolean>} 是否成功取消（false 表示请求已完成或不存在） */
export function cancelOcrRequest(requestId) {
return invoke("cancel_ocr_request", {requestId});
}

/** 0.17.5：OCR 诊断——返回已安装语言列表、引擎语言、中文包状态。 */
export function ocrDiagnose() {
    return invoke("ocr_diagnose");
}

/** 通过 Win32 ShellExecuteW 打开 URL / 协议（比 window.open 更可靠，支持 ms-settings: 等）。 */
export function openUrl(url) {
    return invoke("open_url", {url});
}

/** 0.18.6：在外部终端中执行命令（wt.exe 优先，cmd.exe fallback）。 */
export function runInTerminal(command) {
    return invoke("run_in_terminal", {command});
}

/**
 * 0.11.9-d：翻译文本——直调 translate 插件的 tool（绕过 AI 路径）。
 * @param {string} text 待翻译文本
 * @param {string?} targetLang 目标语言代码(zh/en/ja/ko);省略走插件 setting 默认值
 * @returns {Promise<string>} 译文
 */
export function translateText(text, targetLang) {
    return invoke("translate_text", {text, targetLang: targetLang ?? null});
}

/**
 * 0.11.10-g:批量翻译多行文本。输入 `lines[i]` → 输出 `results[i]`。
 * 单行失败降级到原文（无错误传播），保序。
 */
export function translateLines(lines, targetLang) {
    return invoke("translate_lines", {lines, targetLang: targetLang ?? null});
}

// ── 0.16.3 内容编辑器 ──

/** 打开内容编辑器窗口。payload 为 EditableContentPayload（camelCase）。 */
export function openContentEditor(payload) {
    return invoke("open_content_editor", {payload});
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
    return invoke("copy_clipboard_image", {imageId});
}

/** 0.16.5：将剪贴板图片钉到桌面（pin 窗口）。imageId 为 clipboard_images 表 id。 */
export function pinClipboardImage(imageId) {
    return invoke("pin_clipboard_image", {imageId});
}

// ── 0.16.7-0.16.8 桌面便签 ──

/** 0.16.7：创建便签。返回 StickyNote（含 id）。前端拿到 id 后调 showStickyWindow 打开窗口。 */
export function createStickyNote(content, color) {
    return invoke("create_sticky_note", {content: content ?? "", color: color ?? null});
}

/** 0.16.7：获取单条便签。 */
export function getStickyNote(id) {
    return invoke("get_sticky_note", {id});
}

/** 0.16.7：列出全部便签（按 updated_at 倒序）。 */
export function listStickyNotes() {
    return invoke("list_sticky_notes");
}

/** 0.16.7：更新便签内容（前端防抖 500ms 后调用）。
 *  0.20.7：返回新的 `updated_at`（Unix 秒），前端据此跟踪 revision。 */
export function updateStickyContent(id, content, expectedUpdatedAt) {
    return invoke("update_sticky_content", {id, content, expectedUpdatedAt: expectedUpdatedAt ?? null});
}

/** 0.16.7：更新便签外观（颜色 + 可选格式）。
 *  0.20.7：返回新的 `updated_at`。 */
export function updateStickyAppearance(id, color, format) {
    return invoke("update_sticky_appearance", {id, color, format: format ?? null});
}

/** 0.16.7：更新便签窗口几何（位置 + 尺寸，物理像素）。
 *  0.20.7：返回新的 `updated_at`。 */
export function updateStickyGeometry(id, x, y, width, height) {
    return invoke("update_sticky_geometry", {id, x, y, width, height});
}

/** 0.16.7：设置便签可见性（关闭 = 隐藏）。
 *  P0-1：返回新的 `updated_at`。 */
export function setStickyVisible(id, visible) {
    return invoke("set_sticky_visible", {id, visible});
}

/** 0.16.7：设置便签置顶。
 *  P0-1：返回新的 `updated_at`。 */
export function setStickyAlwaysOnTop(id, alwaysOnTop) {
    return invoke("set_sticky_always_on_top", {id, alwaysOnTop});
}

/** 0.16.7：删除便签（永久）。 */
export function deleteStickyNote(id) {
    return invoke("delete_sticky_note", {id});
}

/** 0.16.7：获取便签统计 { count, visible }。 */
export function getStickyStats() {
    return invoke("get_sticky_stats");
}

/** 0.16.8：显示便签窗口（后端创建/复用 Tauri 窗口）。
 *  0.18.4：可选 atCursor=true 时，新便签定位到鼠标处（标题栏中心对准光标）。
 */
export function showStickyWindow(stickyId, atCursor = false) {
    return invoke("show_sticky_window_cmd", {stickyId, atCursor});
}

/** 0.16.8：销毁便签窗口（删除数据后调用）。 */
export function destroyStickyWindow(stickyId) {
    return invoke("destroy_sticky_window_cmd", {stickyId});
}

/** 0.16.10：显示便签管理窗口。 */
export function showStickyManager() {
    return invoke("show_sticky_manager_cmd");
}

// ── 0.17.7 便签回收站 ──────────────────────────────────

/** 0.17.7：将便签移入回收站（软删除）。 */
export function trashStickyNote(id) {
    return invoke("trash_sticky_note", {id});
}

/** 0.20.0：原子关闭便签。空内容→物理删除，非空→保存最终内容并移入回收站。
 * 返回 { kind: "deleted_empty" | "trashed" }。失败时窗口不关闭。 */
export function closeStickyNote(id, finalContent, expectedUpdatedAt) {
    return invoke("close_sticky_note", {
        id,
        finalContent: finalContent ?? "",
        expectedUpdatedAt: expectedUpdatedAt ?? null,
    });
}

/** 0.17.7：从回收站恢复便签。 */
export function restoreStickyNote(id) {
    return invoke("restore_sticky_note", {id});
}

/** 0.17.7：列出回收站中的便签。 */
export function listTrashedStickyNotes() {
    return invoke("list_trashed_sticky_notes");
}

/** 0.17.7：清空回收站。返回删除的行数。 */
export function clearTrashedStickyNotes() {
    return invoke("clear_trashed_sticky_notes");
}
