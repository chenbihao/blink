//! 编辑器会话纯决策函数（0.23.19-fix 自 index.js / scroll/index.js / ss-output.js 抽出）。
//!
//! 这些判定是 0.23.19 修复的回归缺陷核心（长图误判吞 ESC、文本框守卫吞
//! Alt、拖选中断翻写 source 后取消走错分支），此前只能用源码字符串断言锁
//! 实现——重命名/格式化即失效，且"源码里有这行字"不等于"行为正确"。
//! 抽成无 DOM/IPC 依赖的纯函数后由 ss-editor-esc.test.mjs 直接行为测试；
//! 监听注册/接线层仍由该测试的源码断言兜底。

import {IMAGE_SOURCE} from './image-editor-session.js';

/**
 * 目标是否文本输入类控件（快捷键需让位——Alt+数字在文本框里是
 * Windows Alt 码合法输入）。range/checkbox 等非文本 input 不算：拖完滑杆
 * 焦点滞留其上时，Alt 系快捷键应正常响应而非被吞。
 */
export function isTextEntryTarget(el) {
    if (!el) return false;
    if (el.isContentEditable) return true;
    if (el.tagName === 'TEXTAREA') return true;
    if (el.tagName !== 'INPUT') return false;
    const type = (el.getAttribute('type') || 'text').toLowerCase();
    return !['range', 'checkbox', 'radio', 'button', 'submit', 'reset', 'file', 'image', 'color'].includes(type);
}

/**
 * 编辑器「一次 ESC 整体退出」的例外判定——取色器/文本输入/IME 各有自己的
 * ESC 语义，不做整体退出。窗口捕获层与 document 冒泡层共用，保证两层行为一致。
 *
 * @param {KeyboardEvent} e - 事件（或事件形态兼容的对象）
 * @param {{eyedropperActive?: boolean}} [state] - 调用方注入的会话态
 */
export function editorEscExempt(e, {eyedropperActive = false} = {}) {
    if (e.isComposing) return true;
    if (eyedropperActive) return true;
    const tgt = e.target;
    return Boolean(tgt && (tgt.tagName === 'INPUT' || tgt.tagName === 'TEXTAREA' || tgt.isContentEditable));
}

/**
 * `_imagePan` 是否属于长截图会话。所有 canvas 编辑器（长图/剪贴板/历史/pin）
 * 都会被设置 _imagePan（图片平移用），不能据此判定"长截图活跃"——否则普通
 * 编辑器里 ESC 首按会被"退出长截图"分支吃掉，第二次才真正取消。
 */
export function isLongScreenshotPan(imagePan, source) {
    return Boolean(imagePan) && source === IMAGE_SOURCE.LONG_SCREENSHOT;
}

/**
 * doCancel 是否走图片编辑器取消分支（imageEditorCancel，含 restore_demoted_pin）。
 * body 的 image-editor-mode 标记兜底：编辑器会话中的拖选中断（abortSelectionInteraction）
 * 曾把 source 翻写成 screenshot，只看 source 会绕过 pin 恢复。
 */
export function cancelIsImageEditorSession(source, imageEditorMode) {
    return source === IMAGE_SOURCE.CLIPBOARD
        || source === IMAGE_SOURCE.HISTORY
        || source === IMAGE_SOURCE.PIN
        || Boolean(imageEditorMode);
}
