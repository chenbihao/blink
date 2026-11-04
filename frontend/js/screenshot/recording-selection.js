/** 录制准备态选区：复用截图的移动/八向缩放几何，不恢复静态截图底图。 */
import {computeInteractionRect} from './ss-interaction.js';
import {findDisplayCssAt} from './ss-display.js';

export function bindRecordingSelection({frame, state, ss, onChange, onInvalidate, onInteraction}) {
    let drag = null;
    const owns = d => state.preparing && !state.active && state.owns(d.epoch, null);
    function update(d, event, commit = false) {
        if (!owns(d)) return;
        d.lastEvent = event;
        const dx = event.clientX - d.startX, dy = event.clientY - d.startY;
        if (!d.moved && Math.hypot(dx, dy) < 3) return;
        if (!d.moved) { d.moved = true; onInvalidate(); }
        // 手柄位于边框外侧；以按下点的增量计算，避免首次移动时选区跳动。
        const x = d.kind === 'resize' ? d.edgeX + dx : event.clientX;
        const y = d.kind === 'resize' ? d.edgeY + dy : event.clientY;
        const meta = window.__blinkScreenMeta || {};
        const rect = computeInteractionRect(d, x, y, !!event.shiftKey,
            {meta, canvasW: ss.canvas?.width || meta.w, canvasH: ss.canvas?.height || meta.h});
        onChange(rect, commit);
    }
    function finish(event, cancelled = false) {
        const d = drag;
        if (!d || (event && event.pointerId !== d.pointerId)) return;
        if (owns(d)) {
            if (cancelled && d.moved) onChange(d.original, true);
            else if (event) update(d, event, true);
            onInteraction(false, d.epoch);
        }
        drag = null;
        try { frame.releasePointerCapture(d.pointerId); } catch {}
    }
    frame.addEventListener('pointerdown', event => {
        if (event.button !== 0 || drag || !state.preparing || state.active || !ss.selCss) return;
        const handle = event.target.closest('[data-recording-resize]')?.dataset.recordingResize;
        const edge = event.target.closest('[data-recording-edge]');
        if (!handle && !edge) return;
        event.preventDefault(); event.stopPropagation();
        const original = {...ss.selCss};
        drag = {kind: handle ? 'resize' : 'move', handle, original, epoch: state.epoch,
            pointerId: event.pointerId, startX: event.clientX, startY: event.clientY, moved: false, lastEvent: event,
            edgeX: handle?.includes('w') ? original.x : original.x + original.w,
            edgeY: handle?.includes('n') ? original.y : original.y + original.h,
            monitor: findDisplayCssAt(original.x + original.w / 2, original.y + original.h / 2)};
        try { frame.setPointerCapture(event.pointerId); } catch { drag = null; return; }
        onInteraction(true, drag.epoch);
    });
    frame.addEventListener('pointermove', event => { if (drag && event.pointerId === drag.pointerId) update(drag, event); });
    frame.addEventListener('pointerup', event => finish(event));
    frame.addEventListener('pointercancel', event => finish(event, true));
    frame.addEventListener('lostpointercapture', event => finish(event, true));
    return {cancel: () => finish(null, true), commit: () => { if (drag) finish(drag.lastEvent); }};
}
