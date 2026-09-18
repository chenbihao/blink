//! 截图 overlay 指针输入工具（0.23.15）。
//!
//! 主画布拖选统一走 Pointer Events。这里放与具体 DOM 结构无关的纯逻辑：
//! pointer capture 起止、合并采样点归一、以及"异常中断"的判定。
//! 抽成独立模块的原因是把这几条路径变成可单元测试的对象——`index.js` 是
//! 编排层（模块加载即绑事件），在 Node 里不可导入。
//!
//! 迁移背景：此前拖选监听挂在 MouseEvent 上，`e.pointerId` 恒为 undefined，
//! `setPointerCapture` 分支从不生效，"快速拖出 canvas 边缘再松手"只能靠
//! window 层兜底。

/**
 * 取本次指针事件的**最终采样点**。
 *
 * WebView2 会在一帧内合并多个采样点，`getCoalescedEvents()` 能拿到全部历史点。
 * 选区只关心"现在在哪"：逐个重放历史点只会把同一帧的过期几何重复算一遍，
 * 反而放大掉帧。因此只取最后一个采样点。
 *
 * 事件未提供该 API（或调用抛错）时退回原始事件。
 */
export function latestPointerSample(e) {
    if (typeof e?.getCoalescedEvents !== 'function') return e;
    try {
        const samples = e.getCoalescedEvents();
        if (samples && samples.length > 0) return samples[samples.length - 1];
    } catch (_) {
        // 未实现或调用失败：退回原始事件
    }
    return e;
}

/**
 * 把指针事件归一为「最新采样点 + 修饰键 + pointerId」。
 *
 * 修饰键取外层事件：合帧后的采样点不保证携带稳定的键盘态，而 Shift 决定
 * 是否强制 1:1 正方形，必须以外层事件为准。
 */
export function pointerPoint(e) {
    const sample = latestPointerSample(e);
    return {
        offsetX: sample.offsetX,
        offsetY: sample.offsetY,
        shiftKey: e?.shiftKey,
        pointerId: e?.pointerId,
    };
}

/**
 * 起 pointer capture：指针移出元素/窗口后仍能收到 pointerup。
 * 幂等且容错——指针已释放或环境不支持时静默降级为无 capture 的原有行为。
 */
export function capturePointer(target, e) {
    if (!target || e?.pointerId === undefined) return false;
    if (typeof target.setPointerCapture !== 'function') return false;
    try {
        target.setPointerCapture(e.pointerId);
        return true;
    } catch (_) {
        return false;
    }
}

/**
 * 释放 pointer capture（幂等）。
 * 只在自己仍持有 capture 时释放，避免误释放其它元素的 capture。
 */
export function releasePointer(target, e) {
    if (!target || e?.pointerId === undefined) return false;
    try {
        if (typeof target.hasPointerCapture === 'function' && !target.hasPointerCapture(e.pointerId)) {
            return false;
        }
        if (typeof target.releasePointerCapture !== 'function') return false;
        target.releasePointerCapture(e.pointerId);
        return true;
    } catch (_) {
        return false;
    }
}

/**
 * 指针丢失（pointercancel / lostpointercapture）时是否要按"取消拖选"处理。
 *
 * **必须存在未结束的拖选状态才返回 true**：正常 pointerup 之后浏览器也会派发
 * lostpointercapture，此时交互状态已被清空；若不判状态就取消，会把刚提交的
 * 选区二次取消，用户会看到选区莫名其妙回到上一个矩形。
 */
export function hasActiveDragInteraction({selectionInteraction, isDragging, pendingSnap}) {
    return !!selectionInteraction || !!isDragging || !!pendingSnap;
}
