//! OCR 按钮忙碌态呼吸动效（方案 A：图标呼吸）。
//!
//! 状态源（只读消费，不反向控制）：
//! - `ss.ocrPrewarmActive` —— OCR 预热请求在途（后台自动识别）
//! - `ss.ocrBusy`         —— 用户点[识别]/[翻译]等待中
//!
//! 两者任一为 true 时给 `#btn-ocr` 挂 `is-busy` class，CSS 层做图标呼吸动画。
//! 防闪烁：加 class 延迟 200ms（WinRT 预热几百 ms 就完成的短任务不闪动画）；
//! 解除则立即摘 class，识别结束动效即消失。

import {ss} from './ss-state.js';

const OCR_BUSY_DELAY_MS = 200;

let busyDelayTimer = null;

function ocrBusyNow() {
    return Boolean(ss.ocrBusy || ss.ocrPrewarmActive);
}

/**
 * 同步 #btn-ocr 的 is-busy class 到当前 OCR 忙碌态。
 *
 * 调用点：
 * - updateOutputButtonsDisabled()（所有 ocrBusy 翻转处，集中收口）
 * - triggerOcrPrewarm / 预热 finally（index.js）
 * - cancelActiveOcr()（ESC / 重选 / 会话退出）
 *
 * 无 #btn-ocr（如预热窗口未建工具栏）时静默跳过。
 */
export function updateOcrButtonBusy() {
    const btn = document.getElementById('btn-ocr');
    if (!btn) return;
    if (ocrBusyNow()) {
        if (busyDelayTimer === null && !btn.classList.contains('is-busy')) {
            busyDelayTimer = setTimeout(() => {
                busyDelayTimer = null;
                // 延迟期间可能已被取消/退出会话，摘挂前再校验一次
                if (!ocrBusyNow()) return;
                document.getElementById('btn-ocr')?.classList.add('is-busy');
            }, OCR_BUSY_DELAY_MS);
        }
        return;
    }
    if (busyDelayTimer !== null) {
        clearTimeout(busyDelayTimer);
        busyDelayTimer = null;
    }
    btn.classList.remove('is-busy');
}
