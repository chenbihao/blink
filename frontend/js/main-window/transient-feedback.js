//! TransientFeedback（0.24 §5.5）：loading / success / error 瞬态反馈。
//!
//! 承载此前散落三处的瞬态提示：action-error 的动作错误（旧 .action-error-hint
//! 叠在 statusbar 上）、clipboard-mode 的批量复制 loading/success/error（旧直接
//! 改写 statusbar 内容）、command-mode 的执行错误（旧 #command-hint error 态）。
//!
//! 底部绝对定位覆盖层，不占布局、不触发窗口 resize；后到的反馈替换先到的
//! （不叠加），duration 到期自动消失。持续态（如 loading）传 duration = 0，
//! 由下一个反馈或 dismiss() 收场。

const DEFAULT_DURATION_MS = 2500;
const ERROR_DURATION_MS = 3500;

let el = null;
let timer = null;

/** 初始化：绑定 DOM。main.js 启动时调一次。 */
export function init() {
    el = document.getElementById("transient-feedback");
}

/**
 * 显示一条瞬态反馈。
 * @param {string} message 已本地化的消息文本
 * @param {{type?: "info"|"success"|"error", durationMs?: number}} [opts]
 *   type 影响配色（error 走 accent 强调色）；durationMs=0 表示持续显示直到被替换
 */
export function show(message, opts = {}) {
    if (!el) return;
    if (timer) {
        clearTimeout(timer);
        timer = null;
    }
    el.textContent = message;
    el.classList.remove("hidden", "feedback-info", "feedback-success", "feedback-error");
    el.classList.add(opts.type === "error" ? "feedback-error" : opts.type === "success" ? "feedback-success" : "feedback-info");
    el.setAttribute("role", opts.type === "error" ? "alert" : "status");

    const duration = opts.durationMs ?? (opts.type === "error" ? ERROR_DURATION_MS : DEFAULT_DURATION_MS);
    if (duration > 0) {
        timer = setTimeout(dismiss, duration);
    }
}

/** 立即消失（loading 被 success/error 接替前由调用方使用）。 */
export function dismiss() {
    if (!el) return;
    if (timer) {
        clearTimeout(timer);
        timer = null;
    }
    el.classList.add("hidden");
    el.textContent = "";
}
