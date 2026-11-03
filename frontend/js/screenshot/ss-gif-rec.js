//! 录制叠层模式（0.28.23 §5.7.1/§5.7.2）——统一浮栏与实时涂画。
//!
//! 录制开始后 chord-screenshot overlay **不隐藏**，转为录制叠层：
//! - 静态截图层清空、遮罩/选区层隐藏、body 透明——桌面经 DWM 透出；
//! - 同一 `#toolbar` 宿主切到录制面（`toolbar-mode-gif-rec`），共享标注工具
//!   （画笔/箭头/矩形/椭圆/文字/数字/高亮/橡皮 + 撤销/重做/清空）；
//! - `#annot-canvas` 即实时涂画层：坐标相对录制区域（原选区），标注引擎
//!   零改动复用；涂画内容经 DWM 合成进入 BitBlt 采集帧（半透明仅合成一次），
//!   进行中的笔画每帧重放自然入帧；
//! - 「操作桌面」模式 = overlay 整窗鼠标穿透（后端 `gif_overlay_input`），
//!   指针落在命中矩形（工具栏 + 打开的弹层）内时后端轮询临时取消穿透；
//! - 「移动区域」按钮按住拖动 → `gif_move_recording_region`，涂画层与
//!   范围框随区域移动（涂画坐标相对区域，天然跟随）。
//!
//! Esc 语义：录制中不退出（index.js 全局链在录制态短路）；取消走受保护的
//! 二次确认按钮。

import {ss} from './ss-state.js';
import {cssPointToScreen, cssRectToBitmap, getRenderScale, screenRectToCss} from './ss-selection-geometry.js';
import {positionToolbar} from './ss-display.js';
import {selectTool} from './ss-toolbar.js';
import {invoke, listen, normalizeError} from '../shared/tauri.js';
import {EVENTS} from '../shared/event-names.js';
import {showTransientHint} from './ss-ocr.js';
import * as annot from './annotation-engine.js';
import {formatTimecode} from '../gif/seek-bar.js';
import {beginAudioRecording, endAudioRecording, applyAudioStatus, closeAudioSettings} from './ss-gif-audio.js';

/** 宿主录制面 class（CSS 面切换规则）。 */
const REC_MODE_CLASS = 'toolbar-mode-gif-rec';
/** body 录制叠层 class（层透明化）。 */
const BODY_REC_CLASS = 'gif-recording';
/** 暂停态 class（状态点变色）。 */
const PAUSED_CLASS = 'gif-rec-paused';
/** 录制叠层不可用的已有标注类型（依赖截图底图像素）。 */
const UNSUPPORTED_TYPES = new Set(['blur', 'pixelate', 'mosaic', 'magnifier', 'spotlight', 'spotlight-multi']);
/** 录制叠层可用的绘制工具（进入录制面时当前工具不可用则切画笔）。 */
const REC_TOOLS = new Set(['pencil', 'arrow', 'rect', 'ellipse', 'highlight-multiply', 'highlight-translucent', 'text', 'number', 'eraser']);
/** 命中矩形周期上报间隔（ms）。 */
const HIT_RECT_INTERVAL_MS = 250;
/** 状态时长插值间隔（ms）。 */
const TICK_INTERVAL_MS = 100;
/** 取消确认按钮的回位时间（ms）。 */
const CANCEL_ARM_MS = 3000;

/** 模块状态（会话真源在后端；此为 UI 投影）。 */
const state = {
    active: false,
    sessionId: null,
    phase: 'recording',
    elapsedBase: 0,
    elapsedAt: 0,
    frames: 0,
    tickTimer: 0,
    /** 录制区域（物理屏幕矩形；GIF_REGION_UPDATED 对账）。 */
    regionPhys: null,
    /** 虚拟屏边界（物理像素）。 */
    vs: null,
    desktopMode: false,
    hitRectTimer: 0,
    moveDrag: null,
    cancelArmed: false,
    cancelTimer: 0,
    stopping: false,
    textInputs: new Set(),
    resumeAfterText: false,
};

function toolbarEl() {
    return ss.toolbar || document.getElementById('toolbar');
}

function el(id) {
    return document.getElementById(id);
}

/** 录制叠层是否生效（UI 判定；后端会话存在性见 state.active）。 */
export function isGifRecActive() {
    return state.active;
}

/** 纯函数：命令列表中依赖截图底图、无法迁移进录制的类型（去重）。 */
export function unsupportedKindsOf(commands, watermarkPresent) {
    const kinds = new Set();
    for (const cmd of commands || []) {
        if (UNSUPPORTED_TYPES.has(cmd.type)) kinds.add(cmd.type);
    }
    if (watermarkPresent) kinds.add('watermark');
    return [...kinds];
}

/** 已有标注中依赖截图底图、无法迁移进录制的类型清单（准备态开录前检查用）。 */
export function unsupportedAnnotationKinds() {
    return unsupportedKindsOf(annot.getCommands(), annot.hasWatermark());
}

/** 准备态开录前的可迁移检查：存在未适配效果时列出并阻止（§5.7.2）。 */
export function annotationsMigratable() {
    const kinds = unsupportedAnnotationKinds();
    if (kinds.length) {
        showTransientHint(
            `存在录制不支持的标注效果（${kinds.join('、')}），请先撤销或清除后再开始录制`,
            {isError: true, duration: 4000},
        );
        return false;
    }
    return true;
}

/**
 * 进入录制叠层模式（`gif_start_recording` 成功后由准备态调用）。
 * @param {{x:number, y:number, w:number, h:number}} regionPhys 录制区域（物理屏幕坐标）
 */
export async function enterGifRecordOverlay(regionPhys, sessionId) {
    await beginAudioRecording(sessionId);
    const toolbar = toolbarEl();
    if (!toolbar) throw new Error('录制工具栏未就绪');
    state.active = true;
    state.sessionId = sessionId;
    state.stopping = false;
    state.textInputs.clear();
    state.resumeAfterText = false;
    state.desktopMode = false;
    state.regionPhys = {...regionPhys};
    state.phase = 'recording';
    state.elapsedBase = 0;
    state.elapsedAt = performance.now();
    state.frames = 0;
    state.moveDrag = null;
    resetCancel();
    const meta = window.__blinkScreenMeta || {};
    state.vs = {vx: meta.vx, vy: meta.vy, vw: meta.w, vh: meta.h};

    toolbar.classList.remove('toolbar-mode-gif-prep');
    toolbar.classList.add(REC_MODE_CLASS);
    document.body.classList.add(BODY_REC_CLASS);
    document.documentElement.classList.add(BODY_REC_CLASS);
    document.body.classList.remove(PAUSED_CLASS);

    // 静态截图层清空（保留元素——它是指针事件承接面；位图清空即透明）。
    // OCR/翻译嵌图等 overlay 图层不属于涂画，随底图一起退出画面。
    const base = el('canvas');
    if (base && base.width > 0) {
        base.getContext('2d').clearRect(0, 0, base.width, base.height);
    }
    // 涂画层脱离截图底图：清除底图引用（依赖底图的效果已被禁用），画布
    // 尺寸/命令栈保持——截图态已画标注按区域局部坐标无缝延续。
    const annotCanvas = ss.annotCanvas || el('annot-canvas');
    if (annotCanvas) annot.updateCropData(null, annotCanvas.width, annotCanvas.height);

    // 当前工具不可用（select/底图效果）→ 切画笔（录制涂画的默认工具）
    if (!REC_TOOLS.has(annot.getTool())) selectTool('pencil');

    // 工具栏若停在录制区域内（用户拖过）会入成品：清除用户位置，回到
    // 默认跟随（选区下方 → 上方），避免工具栏被录进画面
    if (toolbar.dataset.userMoved === 'true' && ss.selCss && rectsIntersect(toolbar.getBoundingClientRect(), ss.selCss)) {
        delete toolbar.dataset.userMoved;
    }
    positionToolbar({...ss.selCss}, true);

    syncRegionVisual();
    el('gif-region-frame')?.classList.remove('hidden');
    await bindListenersOnce();
    if (!state.active || state.sessionId !== sessionId) return;

    // 文字输入适配钩子（ss-toolbar showTextInput 消费）：输入框外置出录制
    // 区域 + 编辑期间申请 overlay 键盘焦点（不可聚焦不抢被录应用焦点）。
    ss.gifRecHooks = {
        allowsTool: tool => REC_TOOLS.has(tool),
        adjustTextPlacement,
        onTextInputOpen: openRecordingTextInput,
    };

    // 默认「操作桌面」模式（§5.7.2：默认鼠标穿透，已有涂画继续显示）
    setDesktopMode(true);
    renderRecState();
    startTick();
    // 两次 rAF 之间完成一次透明叠层绘制，再通知采集线程 Start。
    await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    if (!state.active || state.sessionId !== sessionId) return;
    await invoke('gif_overlay_ready', {sessionId});
    const snapshot = await invoke('gif_recorder_state');
    if (state.active && state.sessionId === sessionId) applyRecorderState(snapshot || {});
}

/** 录制会话结束（停止/取消由后端收场并隐藏 overlay）——仅复位 UI 投影。 */
function endOverlaySession() {
    endAudioRecording();
    state.active = false;
    state.sessionId = null;
    state.stopping = false;
    clearInterval(state.tickTimer);
    state.tickTimer = 0;
    clearInterval(state.hitRectTimer);
    state.hitRectTimer = 0;
    state.moveDrag = null;
    resetCancel();
    document.body.classList.remove(BODY_REC_CLASS, PAUSED_CLASS);
    document.documentElement.classList.remove(BODY_REC_CLASS);
    toolbarEl()?.classList.remove(REC_MODE_CLASS);
    el('gif-region-frame')?.classList.add('hidden');
    delete ss.gifRecHooks;
}

function rectsIntersect(a, b) {
    return a.x < b.x + b.w && a.x + a.width > b.x && a.y < b.y + b.h && a.y + a.height > b.y;
}

/** 纯函数：区域拖动的本地钳制（虚拟屏内；后端钳制回发为最终裁决）。 */
export function clampRegionMove(origin, dx, dy, vs) {
    let x = origin.x + dx;
    let y = origin.y + dy;
    if (vs && typeof vs.vx === 'number') {
        x = Math.max(vs.vx, Math.min(x, vs.vx + vs.vw - origin.w));
        y = Math.max(vs.vy, Math.min(y, vs.vy + vs.vh - origin.h));
    }
    return {x, y};
}

// ── 状态显示（GIF_RECORDER_STATE 事件 + 时长插值） ───────────────────────

let listenersReady;
function bindListenersOnce() {
    if (listenersReady) return listenersReady;
    listenersReady = Promise.all([listen(EVENTS.GIF_RECORDER_STATE, event => {
        if (!state.active) return;
        applyRecorderState(event.payload || {});
    }), listen(EVENTS.GIF_REGION_UPDATED, event => {
        if (!state.active) return;
        applyRegionUpdated(event.payload || {});
    })]);
    return listenersReady;
}

function applyRecorderState(p) {
    if (p.sessionId !== state.sessionId) return;
    if (p.phase === 'idle') {
        endOverlaySession();
        return;
    }
    // 会话已收场（正常/取消/异常）——UI 冻结在收尾态，等 overlay 隐藏或
    // 下一轮截图会话 reset。
    if (p.phase === 'done') {
        // 后端已收场/即将隐藏 overlay；停止响应输入
        state.stopping = true;
        renderRecState();
        return;
    }
    if (p.phase === 'stopping' || state.stopping) {
        state.stopping = true;
        renderRecState();
        return;
    }
    state.phase = p.phase;
    applyAudioStatus(p.audio, state.sessionId);
    state.elapsedBase = p.elapsedMs ?? state.elapsedBase;
    state.elapsedAt = performance.now();
    state.frames = p.frames ?? state.frames;
    state.storage = p.storage;state.targetFps = p.targetFps;state.actualFps = p.actualFps;
    renderRecState();
}

function renderRecState() {
    const stateEl = el('gif-rec-state');
    const timeEl = el('gif-rec-time');
    const framesEl = el('gif-rec-frames');
    const pauseBtn = el('gif-rec-pause');
    if (!stateEl) return;
    if (state.stopping) {
        stateEl.textContent = '正在收尾…';
        document.body.classList.remove(PAUSED_CLASS);
        setButtonsDisabled(true);
        return;
    }
    setButtonsDisabled(false);
    const paused = state.phase === 'paused';
    document.body.classList.toggle(PAUSED_CLASS, paused);
    stateEl.textContent = paused ? (state.storage?.pauseReason === 'low_space' ? '空间不足 · 已暂停' : state.storage?.pauseReason === 'space_unavailable' ? '空间查询失败 · 已暂停' : '已暂停（可继续或停止）') : '录制中';
    if (timeEl) timeEl.textContent = formatTimecode(currentElapsedMs());
    if (framesEl) {
        framesEl.textContent = state.targetFps ? `${Number(state.actualFps || 0).toFixed(1)}/${state.targetFps} fps` : `${state.frames} 帧`;
        framesEl.title = `实际 / 目标帧率 · 已采集 ${state.frames} 帧（包含合并的重复画面）`;
    }
    const storageEl=el('gif-rec-storage'), storage=state.storage;
    if(storageEl && storage) {
        const mib=n=>(n/1024/1024).toFixed(1);
        storageEl.textContent=`${mib(storage.materialBytes)} MiB · ${storage.disk ? '余 '+(storage.disk.availableBytes/1024**3).toFixed(1)+' GiB':'空间未知'}${storage.warning && !storage.pauseReason ? ' · 空间偏低' : ''}`;
        storageEl.title=`素材盘 ${storage.disk?.volume || '未知'}\n声音 ${mib(storage.audioBytes)} MiB\n${storage.bytesPerSecond ? mib(storage.bytesPerSecond)+' MiB/s':'增长速度尚未稳定'}\n预留 ${mib(storage.reserveBytes)} MiB`;
    }
    if (pauseBtn) {
        const icons = pauseBtn.querySelectorAll('.icon');
        if (icons.length === 2) {
            icons[0].classList.toggle('hidden', paused);
            icons[1].classList.toggle('hidden', !paused);
        }
        pauseBtn.title = paused ? '继续录制' : '暂停录制';
    }
}

function currentElapsedMs() {
    if (state.phase !== 'recording') return state.elapsedBase;
    return state.elapsedBase + (performance.now() - state.elapsedAt);
}

function startTick() {
    clearInterval(state.tickTimer);
    state.tickTimer = setInterval(() => {
        if (!state.active || state.stopping) {
            clearInterval(state.tickTimer);
            return;
        }
        const timeEl = el('gif-rec-time');
        if (timeEl && state.phase === 'recording') {
            timeEl.textContent = formatTimecode(currentElapsedMs());
        }
    }, TICK_INTERVAL_MS);
}

function setButtonsDisabled(disabled) {
    for (const id of ['gif-rec-pause', 'gif-rec-stop', 'gif-rec-cancel', 'gif-rec-move', 'gif-rec-mode']) {
        const btn = el(id);
        if (btn) btn.disabled = disabled;
    }
}

// ── 输入模式（涂画 / 操作桌面）与命中矩形 ────────────────────────────────

/** 命中矩形集合（物理屏幕坐标，含少量余量）：工具栏 + 打开的下拉/弹层。 */
function collectHitRects() {
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const {scaleX, scaleY} = getRenderScale(meta);
    const margin = 4;
    const rects = [];
    const push = dom => {
        if (!dom || dom.classList.contains('hidden')) return;
        const r = dom.getBoundingClientRect();
        if (r.width <= 0 || r.height <= 0) return;
        const p = cssPointToScreen(r.x, r.y, meta);
        rects.push([
            p.x - margin,
            p.y - margin,
            p.x + Math.ceil(r.width * scaleX) + margin,
            p.y + Math.ceil(r.height * scaleY) + margin,
        ]);
    };
    push(toolbarEl());
    push(el('gif-audio-settings'));
    // 打开的下拉（data-open="true"）与浮层
    for (const dom of document.querySelectorAll('#toolbar .dropdown[data-open="true"]')) push(dom);
    push(el('ss-action-menu'));
    push(el('sub-panel'));
    return rects;
}

/** 切换输入模式并上报；操作桌面模式启动命中矩形周期上报。 */
function setDesktopMode(desktop) {
    state.desktopMode = desktop;
    const modeBtn = el('gif-rec-mode');
    const label = el('gif-rec-mode-label');
    if (modeBtn) {
        const icons = modeBtn.querySelectorAll('.icon');
        if (icons.length === 2) {
            icons[0].classList.toggle('hidden', desktop);
            icons[1].classList.toggle('hidden', !desktop);
        }
        modeBtn.title = desktop ? '当前：操作桌面（鼠标穿透）——点击切到涂画' : '当前：涂画——点击切到操作桌面';
    }
    if (label) label.textContent = desktop ? '操作桌面' : '涂画';
    invoke('gif_overlay_input', {desktop, rects: collectHitRects()}).catch(err => {
        console.error('[screenshot] overlay 输入模式上报失败', err);
    });
    clearInterval(state.hitRectTimer);
    state.hitRectTimer = 0;
    if (desktop) {
        state.hitRectTimer = setInterval(() => {
            if (!state.active || !state.desktopMode) {
                clearInterval(state.hitRectTimer);
                state.hitRectTimer = 0;
                return;
            }
            invoke('gif_overlay_hit_rects', {rects: collectHitRects()}).catch(() => {});
        }, HIT_RECT_INTERVAL_MS);
    }
}

// ── 区域（范围框 + 移动 + 对账） ─────────────────────────────────────────

/** 范围框与涂画层随 regionPhys/selCss 同步（selCss 为 UI 真源）。 */
function syncRegionVisual() {
    const frame = el('gif-region-frame');
    const annotCanvas = ss.annotCanvas || el('annot-canvas');
    if (ss.selCss && annotCanvas) {
        annotCanvas.style.left = `${ss.selCss.x}px`;
        annotCanvas.style.top = `${ss.selCss.y}px`;
    }
    if (frame && ss.selCss) {
        frame.style.left = `${ss.selCss.x - 3}px`;
        frame.style.top = `${ss.selCss.y - 3}px`;
        frame.style.width = `${ss.selCss.w}px`;
        frame.style.height = `${ss.selCss.h}px`;
    }
}

/** 后端对账（含拖动钳制结果与新录制会话初始区域）。 */
function applyRegionUpdated(p) {
    if (p.sessionId !== state.sessionId) return;
    if (typeof p.x !== 'number' || typeof p.w !== 'number') return;
    state.regionPhys = {x: p.x, y: p.y, w: p.w, h: p.h};
    state.vs = {vx: p.vx, vy: p.vy, vw: p.vw, vh: p.vh};
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    ss.selCss = screenRectToCss({x: p.x, y: p.y, w: p.w, h: p.h}, meta);
    syncRegionVisual();
}

/** 「移动区域」按钮：按住拖动，尺寸固定；拖完保持拖动前录制/暂停状态。 */
function bindMoveButton() {
    const btn = el('gif-rec-move');
    if (!btn) return;
    btn.addEventListener('pointerdown', e => {
        if (!state.active || state.stopping || e.button !== 0 || !state.regionPhys) return;
        e.preventDefault();
        btn.setPointerCapture(e.pointerId);
        const {scaleX, scaleY} = getRenderScale(window.__blinkScreenMeta);
        state.moveDrag = {
            pointerId: e.pointerId,
            startX: e.clientX,
            startY: e.clientY,
            scaleX, scaleY,
            origin: {...state.regionPhys},
        };
    });
    btn.addEventListener('pointermove', e => {
        const drag = state.moveDrag;
        if (!drag || e.pointerId !== drag.pointerId) return;
        const {vx, vy, vw, vh} = state.vs || {};
        const dx = Math.round((e.clientX - drag.startX) * drag.scaleX);
        const dy = Math.round((e.clientY - drag.startY) * drag.scaleY);
        const clamped = clampRegionMove(drag.origin, dx, dy,
            typeof vx === 'number' ? {vx, vy, vw, vh} : null);
        const nx = clamped.x;
        const ny = clamped.y;
        state.regionPhys = {x: nx, y: ny, w: drag.origin.w, h: drag.origin.h};
        const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
        ss.selCss = screenRectToCss(state.regionPhys, meta);
        syncRegionVisual();
        invoke('gif_move_recording_region', {x: nx, y: ny}).catch(() => {});
    });
    const end = e => {
        const drag = state.moveDrag;
        if (!drag || e.pointerId !== drag.pointerId) return;
        btn.releasePointerCapture?.(e.pointerId);
        state.moveDrag = null;
        // 最后一次上报由后端钳制回发对账
    };
    btn.addEventListener('pointerup', end);
    btn.addEventListener('pointercancel', end);
}

// ── 停止 / 取消（受保护） / 暂停 ────────────────────────────────────────

async function stopRecording() {
    if (state.stopping) return;
    state.stopping = true;
    renderRecState();
    try {
        await invoke('gif_stop_recording');
    } catch (err) {
        console.error('[screenshot] 停止录制失败', err);
        showTransientHint(`停止失败：${normalizeError(err).message}`, {isError: true, duration: 3000});
        state.stopping = false;
        renderRecState();
    }
}

/** 取消并丢弃：受保护的二次确认（首击点亮按钮，3 秒内再击确认）。 */
function resetCancel() {
    clearTimeout(state.cancelTimer);
    state.cancelTimer = 0;
    state.cancelArmed = false;
    el('gif-rec-cancel')?.classList.remove('confirming');
    el('gif-rec-cancel')?.setAttribute('title', '取消并丢弃本次录制（需再次点击确认）');
}

function cancelRecording() {
    if (state.stopping) return;
    const btn = el('gif-rec-cancel');
    if (!state.cancelArmed) {
        state.cancelArmed = true;
        btn?.classList.add('confirming');
        btn?.setAttribute('title', '再次点击确认丢弃本次录制');
        clearTimeout(state.cancelTimer);
        state.cancelTimer = setTimeout(() => {
            state.cancelArmed = false;
            btn?.classList.remove('confirming');
            btn?.setAttribute('title', '取消并丢弃本次录制（需再次点击确认）');
        }, CANCEL_ARM_MS);
        return;
    }
    clearTimeout(state.cancelTimer);
    state.cancelArmed = false;
    btn?.classList.remove('confirming');
    state.stopping = true;
    renderRecState();
    invoke('gif_cancel_recording').catch(err => {
        console.error('[screenshot] 取消录制失败', err);
        state.stopping = false;
        renderRecState();
    });
}

async function togglePause() {
    if (state.stopping) return;
    if (state.phase === 'paused') {
        await closeAudioSettings();
        await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    }
    const cmd = state.phase === 'paused' ? 'gif_resume_recording' : 'gif_pause_recording';
    invoke(cmd).catch(err => {
        console.error('[screenshot] 暂停/继续失败', err);
        showTransientHint(`操作失败：${normalizeError(err).message}`, {isError: true, duration: 3000});
    });
}

// ── 文字输入适配（输入框外置出录制区域 + 焦点申请） ─────────────────────

/** 优先放在选区外，四侧均无空间时保留可见输入位置，由输入生命周期暂停采集。 */
export function placeTextOutsideRegion(cssPos, sel, viewport = {w: Infinity, h: Infinity}, size = {w: 0, h: 0}) {
    if (!sel) return cssPos;
    const clamp = pos => ({
        x: Math.max(8, Math.min(pos.x, viewport.w - size.w - 8)),
        y: Math.max(8, Math.min(pos.y, viewport.h - size.h - 8)),
    });
    const candidates = [
        {x: cssPos.x, y: sel.y + sel.h + 8},
        {x: cssPos.x, y: sel.y - size.h - 8},
        {x: sel.x + sel.w + 8, y: cssPos.y},
        {x: sel.x - size.w - 8, y: cssPos.y},
    ];
    const original = clamp(cssPos);
    if (!textOverlapsRegion(original, sel, size)) return original;
    return candidates.map(clamp).find(pos => !textOverlapsRegion(pos, sel, size)) || original;
}

function textOverlapsRegion(pos, sel, size) {
    return pos.x <= sel.x + sel.w && pos.x + size.w >= sel.x
        && pos.y <= sel.y + sel.h && pos.y + size.h >= sel.y;
}

function adjustTextPlacement(cssPos, size) {
    return placeTextOutsideRegion(cssPos, ss.selCss, {w: window.innerWidth, h: window.innerHeight}, size);
}

/** 返回所属输入框的收尾函数：只恢复由本次文字编辑主动暂停的同一会话。 */
async function openRecordingTextInput(pos, size) {
    const sessionId = state.sessionId;
    const token = {};
    state.textInputs.add(token);
    const current = () => state.active && state.sessionId === sessionId && !state.stopping;
    const release = async () => {
        if (!current() || !state.textInputs.delete(token) || state.textInputs.size) return;
        await invoke('gif_overlay_focusable', {focusable: false});
        if (!state.resumeAfterText) return;
        // 输入框已移除、文字已重绘后才续录，防止捕获编辑控件或提交前的一帧。
        await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
        if (!current() || state.textInputs.size) return;
        const snapshot = await invoke('gif_recorder_state');
        if (!current() || state.textInputs.size) return;
        state.resumeAfterText = false;
        if (snapshot?.sessionId === sessionId && snapshot.phase === 'paused'
            && !snapshot.audio?.error && !snapshot.storage?.pauseReason) {
            await invoke('gif_resume_recording');
        }
    };
    try {
        if (!current()) throw new Error('录制会话已结束');
        if (textOverlapsRegion(pos, ss.selCss, size)) {
            let snapshot = await invoke('gif_recorder_state');
            if (snapshot?.sessionId !== sessionId || !current()) throw new Error('录制会话已切换');
            if (snapshot.phase === 'recording') {
                await invoke('gif_pause_recording');
                const deadline = performance.now() + 3000;
                do {
                    snapshot = await invoke('gif_recorder_state');
                    if (snapshot?.sessionId !== sessionId || !current()) throw new Error('录制会话已结束');
                    if (snapshot.phase === 'paused') { state.resumeAfterText = true; break; }
                    if (snapshot.phase !== 'recording' || performance.now() >= deadline) throw new Error('录制尚未暂停，请重试文字输入');
                    await new Promise(resolve => setTimeout(resolve, 16));
                } while (true);
            } else if (snapshot.phase !== 'paused') {
                throw new Error('请等待录制就绪后再输入文字');
            }
            applyRecorderState(snapshot);
        }
        await invoke('gif_overlay_focusable', {focusable: true});
        if (!current()) throw new Error('录制会话已结束');
        return release;
    } catch (error) {
        await release().catch(() => {});
        showTransientHint(`文字输入失败：${normalizeError(error).message}`, {isError: true, duration: 3000});
        throw error;
    }
}

/** 绑定录制面按钮（index.js 初始化调用一次）。 */
export function bindGifRecBar() {
    void bindListenersOnce();
    bindMoveButton();
    el('gif-rec-mode')?.addEventListener('click', () => {
        if (!state.active || state.stopping) return;
        setDesktopMode(!state.desktopMode);
    });
    el('gif-rec-pause')?.addEventListener('click', togglePause);
    el('gif-rec-stop')?.addEventListener('click', stopRecording);
    el('gif-rec-cancel')?.addEventListener('click', cancelRecording);
    // Esc 由 index.js 的分层链先处理输入框/弹层，再在录制态消费。
}

/** 会话外部收场兜底（overlay 被 hide_screenshot_overlay 隐藏后调用）。 */
export function handleOverlayHidden() {
    endAudioRecording();
    if (state.active) endOverlaySession();
}
