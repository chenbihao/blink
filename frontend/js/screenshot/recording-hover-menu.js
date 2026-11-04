/** 结束按钮的悬浮操作：跨按钮/面板留离开缓冲，键盘可直接展开。 */
export function bindRecordingHoverMenu({trigger, panel, enabled, isOpen, open, close,
    setTimer = setTimeout, clearTimer = clearTimeout}) {
    if (!trigger || !panel) return {sync() {}, reset() {}, dismiss() {}};
    let openTimer = null, closeTimer = null, returningFocus = false;
    function reset() {
        if (openTimer != null) clearTimer(openTimer);
        if (closeTimer != null) clearTimer(closeTimer);
        openTimer = closeTimer = null;
    }
    const contains = target => target && (trigger.contains(target) || panel.contains(target));
    function show(keyboard = false) { reset(); if (enabled()) open(keyboard); }
    function leave(event) {
        reset();
        if (contains(event.relatedTarget)) return;
        closeTimer = setTimer(() => { closeTimer = null; close(); }, 220);
    }
    trigger.addEventListener('pointerenter', () => {
        reset();
        if (enabled() && !isOpen()) openTimer = setTimer(() => { openTimer = null; show(); }, 180);
    });
    trigger.addEventListener('pointerleave', leave);
    panel.addEventListener('pointerenter', reset);
    panel.addEventListener('pointerleave', leave);
    trigger.addEventListener('focusin', () => { if (!returningFocus) show(true); });
    panel.addEventListener('focusin', reset);
    trigger.addEventListener('focusout', leave);
    panel.addEventListener('focusout', leave);
    trigger.addEventListener('keydown', event => {
        if (event.key !== 'ArrowDown' || !enabled()) return;
        event.preventDefault(); show(true);
        panel.querySelector('button:not(:disabled)')?.focus({preventScroll: true});
    });
    panel.addEventListener('click', () => { reset(); close(); });
    return {reset,
        dismiss(restoreFocus = false) {
            reset(); returningFocus = restoreFocus;
            // Esc 收起后同步还焦点给结束按钮，不能因此立即重新展开。
            queueMicrotask(() => { returningFocus = false; });
        },
        sync() { if (!enabled()) { reset(); close(); } },
    };
}
