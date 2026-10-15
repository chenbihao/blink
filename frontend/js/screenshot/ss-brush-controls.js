import {ss, TOOL_CAPS} from './ss-state.js';
import * as annot from './annotation-engine.js';
import {updateStrokeCursor} from './ss-interaction.js';
import {brushDiameter, createSizeWheel, stepSize} from './brush-size.js';

const PREFIX = {stroke: 'stroke-width', brush: 'brush-size'};
const readSize = category => category === 'stroke' ? annot.getStrokeWidth() : annot.getBrushSize();

export function syncSizeControls() {
    for (const [category, prefix] of Object.entries(PREFIX)) {
        const value = readSize(category);
        const dropdown = document.getElementById(`${prefix}-dropdown`);
        dropdown?.querySelectorAll(`[data-${prefix}]`).forEach(item => {
            item.classList.toggle('active', Number(item.dataset[category === 'stroke' ? 'strokeWidth' : 'brushSize']) === value);
        });
        const slider = document.getElementById(`${prefix}-range`);
        const output = document.getElementById(`${prefix}-value`);
        if (slider) slider.value = value;
        if (output) output.textContent = `${value} px`;
        const icon = document.getElementById(`${prefix}-trigger-icon`);
        if (icon) {
            icon.textContent = value;
            if (category === 'stroke') icon.dataset.strokeStyle = annot.getStrokeStyle();
        }
    }
}

/** 点击、滑杆和滚轮共用入口；engine 拒绝绘制途中的尺寸修改。 */
export function applySize(category, value) {
    if (category === 'stroke') annot.setStrokeWidth(value);
    else annot.setBrushSize(value);
    syncSizeControls();
}

export function bindSizeControls() {
    for (const [category, prefix] of Object.entries(PREFIX)) {
        const dropdown = document.getElementById(`${prefix}-dropdown`);
        dropdown?.querySelectorAll(`[data-${prefix}]`).forEach(item => {
            item.addEventListener('click', () => applySize(category,
                item.dataset[category === 'stroke' ? 'strokeWidth' : 'brushSize']));
        });
        const slider = document.getElementById(`${prefix}-range`);
        slider?.addEventListener('input', () => applySize(category, slider.value));
        const wheel = createSizeWheel();
        const onWheel = event => {
            event.preventDefault();
            event.stopPropagation();
            if (ss.isAnnotDragging) { wheel.reset(); return; }
            const count = wheel.steps(event, category);
            let value = readSize(category);
            for (let i = 0; i < Math.abs(count); i++) value = stepSize(category, value, -Math.sign(count));
            if (count) applySize(category, value);
        };
        document.getElementById(`${prefix}-trigger`)?.addEventListener('wheel', onWheel, {passive: false});
        slider?.addEventListener('wheel', onWheel, {passive: false});
        dropdown?.addEventListener('mouseleave', () => wheel.reset());
    }
    syncSizeControls();
}

export function createCanvasSizeWheel() {
    const wheel = createSizeWheel();
    return event => {
        const tool = annot.getTool(), caps = TOOL_CAPS[tool];
        const rect = ss.selCss;
        const mode = annot.getToolMode(tool);
        const eligible = ss.isAnnotating && rect && !ss.sent && !ss.scrollSession.capturing && !ss.scrollSession.captureFinalizing
            && !ss.isAnnotDragging && !ss.isDragging && !ss.selectionInteraction
            && !ss._imagePan?.dragging && !event.ctrlKey && !event.metaKey && !event.altKey
            && !document.documentElement.classList.contains('eyedropper-picking')
            && caps?.hasCursor && (!caps.supportMode || mode === 'brush')
            && ['stroke', 'brush'].includes(caps.widthCat)
            && [ss.canvas, ss.annotCanvas, ss.hitCanvas, ss.interactionCanvas].includes(event.target)
            && event.clientX >= rect.x && event.clientX <= rect.x + rect.w
            && event.clientY >= rect.y && event.clientY <= rect.y + rect.h;
        if (!eligible) { wheel.reset(); return; }
        event.preventDefault();
        event.stopPropagation();
        const count = wheel.steps(event, `${tool}:${mode}:${ss.selectionRevision}`);
        if (!count) return;
        const category = caps.widthCat, previous = readSize(category);
        let value = previous;
        for (let i = 0; i < Math.abs(count); i++) value = stepSize(category, value, -Math.sign(count));
        applySize(category, value);
        updateStrokeCursor(event.clientX, event.clientY);
        if (value !== previous) {
            ss._showTransientHint?.(`${value} px · Ø ${Math.round(brushDiameter(tool, value))} px`, {duration: 800});
        }
    };
}
