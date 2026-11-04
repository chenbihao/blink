//! 录制叠层模式（0.28.23 §5.7.1/§5.7.2）——统一浮栏与实时涂画。
//!
//! 录制开始后 chord-screenshot overlay **不隐藏**，转为录制叠层：
//! - 静态截图层清空、遮罩/选区层隐藏、body 透明——桌面经 DWM 透出；
//! - 同一 `#toolbar` 由 recording-session.mode 投影，原位共享标注工具
//!   （画笔/箭头/矩形/椭圆/文字/数字/高亮/橡皮 + 撤销/重做/清空）；
//! - `#annot-canvas` 即实时涂画层：坐标相对录制区域（原选区），标注引擎
//!   零改动复用；窗口整体排除捕获，标注画布变化经有界二进制快照提交，
//!   采集端在桌面帧上只合成标注一次，再叠加光标；
//! - 「操作桌面」模式 = overlay 整窗鼠标穿透（后端 `gif_overlay_input`），
//!   指针落在命中矩形（工具栏 + 打开的弹层）内时后端轮询临时取消穿透；
//!
//! Esc 语义：录制中不退出（index.js 全局链在录制态短路）；取消走受保护的
//! 二次确认按钮。

import {t} from '../i18n/index.js';
import {createAnnotationUploader} from './recording-annotation.js';
import {ss} from './ss-state.js';
import {recordingSession as state} from './recording-session.js';
import {cssPointToScreen, cssRectToBitmap, getRenderScale, screenRectToCss, syncRenderScale} from './ss-selection-geometry.js';
import {drawFinalSelection} from './ss-draw.js';
import {updateLiveSelection} from './ss-live-selection.js';
import {positionToolbar} from './ss-display.js';
import {retainToolbarAnchor, closeRecordingPanel, openRecordingPanel, setRecordingUiListener, reflowRecordingPanels, showRecordingMessage, renderRecordingUi} from './ss-recording-ui.js';
import {selectTool} from './ss-toolbar.js';
import {invoke, listen, normalizeError} from '../shared/tauri.js';
import {EVENTS} from '../shared/event-names.js';
import {showTransientHint} from './ss-ocr.js';
import * as annot from './annotation-engine.js';
import {formatTimecode} from '../gif/seek-bar.js';
import {beginAudioRecording, endAudioRecording, applyAudioStatus, closeAudioSettings} from './ss-gif-audio.js';
import {createPreparationLease} from './recording-preparation.js';
import {bindRecordingSelection} from './recording-selection.js';

/** 录制叠层不可用的已有标注类型（依赖截图底图像素）。 */
const UNSUPPORTED_TYPES = new Set(['blur', 'pixelate', 'mosaic', 'magnifier', 'spotlight', 'spotlight-multi']);
/** 录制叠层可用的绘制工具（进入录制面时当前工具不可用则切画笔）。 */
const REC_TOOLS = new Set(['pencil', 'arrow', 'rect', 'ellipse', 'highlight-multiply', 'highlight-translucent', 'text', 'number', 'eraser']);
/** 命中矩形周期上报间隔（ms）。 */
const HIT_RECT_INTERVAL_MS = 250;
/** 状态时长插值间隔（ms）。 */
const TICK_INTERVAL_MS = 100;
let annotationUploader = null;
let panelFocusTask = Promise.resolve();
let lastStatusWarning = '';
let regionDrag = null;
let preparationSelection = null, selectionInputTask = Promise.resolve();
const preparation = createPreparationLease((...args) => invoke(...args));

export function waitForRecordingPreparation() { return preparation.ready(); }
export function commitRecordingSelection() { preparationSelection?.commit(); }
function overlayInput(command, args) {
    return invoke(command, {...args, preparationId: state.active ? null : state.preparationId});
}

function installPanelFocus(epoch, sessionId) {
    return focusable => {
        panelFocusTask = panelFocusTask.catch(() => {}).then(async () => {
            if (!state.owns(epoch, sessionId) || state.stopping) return;
            await waitForRecordingPreparation();
            if (!state.owns(epoch, sessionId)) return;
            await overlayInput('gif_overlay_focusable', {focusable: focusable || state.textInputs.size > 0});
        }).catch(error => {
            if (state.owns(epoch, sessionId)) showRecordingMessage(`焦点切换失败：${normalizeError(error).message}`);
        });
    };
}

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
export async function enterGifRecordOverlay(regionPhys, sessionId, {restoring = false, audioOptions} = {}) {
    const toolbar = toolbarEl();
    if (!toolbar) throw new Error('录制工具栏未就绪');
    state.sessionId = sessionId;
    state.setMode('starting');
    const epoch = state.epoch;
    await beginAudioRecording(sessionId, audioOptions);
    if (!state.owns(epoch, sessionId)) return;
    ss._loadGen++;
    state.restoredControlsOnly = restoring;
    state.snapshotRevision = 0;
    state.storage = null;
    state.targetFps = Number(el('gif-prep-fps')?.value) || 15;
    state.actualFps = null;
    state.textInputs.clear();
    state.resumeAfterText = false;
    state.regionPhys = {...regionPhys};
    state.elapsedBase = 0;
    state.elapsedAt = performance.now();
    state.frames = 0;
    resetCancel();
    const meta = window.__blinkScreenMeta || {};

    retainToolbarAnchor();
    closeRecordingPanel();
    toolbar.classList.remove('hidden');
    toolbar.setAttribute('aria-label', t('gif.rec.recordingToolbar'));

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

    positionToolbar({...ss.selCss}, true);

    syncRegionVisual();
    el('gif-region-frame')?.classList.remove('hidden');
    await bindListenersOnce();
    if (!state.active || state.sessionId !== sessionId) return;

    // 文字输入适配钩子（ss-toolbar showTextInput 消费）：输入框外置出录制
    // 区域 + 编辑期间申请 overlay 键盘焦点（不可聚焦不抢被录应用焦点）。
    annotationUploader = annotCanvas && !restoring ? createAnnotationUploader({
        canvas: annotCanvas, sessionId,
        send: (bytes, options) => invoke('gif_overlay_annotation', bytes, options),
        onError: error => {
            if (!state.active || state.sessionId !== sessionId) return;
            void invoke('gif_pause_recording', {sessionId: state.sessionId}).catch(() => {});
            showRecordingMessage(`标注同步失败，录制已请求暂停：${normalizeError(error).message}`);
        },
    }) : null;
    annot.setRenderListener?.(() => annotationUploader?.notify());
    ss.gifRecHooks = {
        onDraw: () => annotationUploader?.notify(),
        onPanelFocus: installPanelFocus(state.epoch, sessionId),
        ...drawingHooks(),
        adjustTextPlacement,
        onTextInputOpen: openRecordingTextInput,
    };

    // 默认「操作桌面」模式（§5.7.2：默认鼠标穿透，已有涂画继续显示）
    setDesktopMode(state.desktopMode);
    renderRecState();
    startTick();
    // 两次 rAF 之间完成一次透明叠层绘制，再通知采集线程 Start。
    await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    if (!state.active || state.sessionId !== sessionId) return;
    // 重载恢复保留后端已有标注，不能用空画布覆盖仍在采集的快照。
    if (!restoring) await annotationUploader?.flush();
    if (!state.active || state.sessionId !== sessionId) return;
    if (!restoring) await invoke('gif_overlay_ready', {sessionId});
    const snapshot = await invoke('gif_recorder_state');
    if (restoring && snapshot?.sessionId !== sessionId) { endOverlaySession(); return; }
    if (state.active && state.sessionId === sessionId) applyRecorderState(snapshot || {});
}

let restoringOverlay = null;
/** 同一后端会话可被重新接管；重复唤起只刷新状态，绝不重启采集或重置截图。 */
export function restoreRecordingOverlay(notify = false) {
    if (state.preparing) return Promise.resolve(true);
    // 自己的启动调用尚未拿到 session id 时，不允许另一个恢复请求抢先接管它。
    if(state.mode==='starting' && !state.active) return Promise.resolve(true);
    if (restoringOverlay) return restoringOverlay;
    restoringOverlay = (async () => {
        const epoch = ss._loadGen;
        const sessionAtRequest = state.sessionId;
        const snapshot = await invoke('gif_recorder_state', {restore: true});
        if (!snapshot?.sessionId || !snapshot.region) {
            // 页面重载未保留原截图/准备 UI 时归还旧准备租约，不能遗留整窗穿透。
            if (snapshot?.preparationId && !state.visible) await invoke('gif_overlay_prepare', {active: false, preparationId: snapshot.preparationId});
            if (state.active && state.sessionId === sessionAtRequest && snapshot?.phase === 'idle') endOverlaySession();
            return state.active;
        }
        if (state.active && state.sessionId !== snapshot.sessionId) return true;
        if (!state.active && epoch !== ss._loadGen) return false;
        if (!state.active || state.sessionId !== snapshot.sessionId) {
            if (state.active) endOverlaySession();
            const meta = snapshot.overlayMeta;
            if (!meta || !(meta.w > 0 && meta.h > 0)) throw new Error('录制窗口几何不可用');
            window.__blinkScreenMeta = meta;
            const base = el('canvas');
            base.width = meta.w; base.height = meta.h;
            syncRenderScale(base, meta);
            ss.selCss = screenRectToCss(snapshot.region, meta);
            ss.sent = true; ss.isAnnotating = true;
            ss._loadGen++;
            const canvas = ss.annotCanvas || el('annot-canvas');
            canvas.width = snapshot.region.w; canvas.height = snapshot.region.h;
            const {scaleX, scaleY} = getRenderScale(meta);
            canvas.style.width = `${canvas.width / scaleX}px`;
            canvas.style.height = `${canvas.height / scaleY}px`;
            canvas.classList.remove('hidden');
            annot.reset(canvas.width, canvas.height, null);
            document.documentElement.classList.remove('screenshot-session-inactive');
            await enterGifRecordOverlay(snapshot.region, snapshot.sessionId, {restoring: snapshot.phase !== 'waiting', audioOptions:snapshot.audioOptions});
        }
        if (!state.active) return false;
        if (state.sessionId === snapshot.sessionId) {
            applyRecorderState(snapshot);
            toolbarEl()?.classList.remove('hidden');
            if (notify) showTransientHint(t('gif.rec.restored'), {duration: 2200});
        }
        return true;
    })().finally(() => {restoringOverlay = null;});
    return restoringOverlay;
}

function drawingHooks() {
    return {
        beginGripDrag: beginRecordingGripDrag,
        allowsTool: tool => (!state.active && tool === 'select') || REC_TOOLS.has(tool),
        onToolSelected: tool => { if (!state.busy && REC_TOOLS.has(tool) && !state.drawing) setDesktopMode(false, {select:false}); },
    };
}
export function prepareRecordingTools() {
    const epoch = state.epoch;
    ss.gifRecHooks = {...drawingHooks(), onPanelFocus: installPanelFocus(epoch, null)};
    const base = el('canvas');
    if (base) base.getContext('2d').clearRect(0, 0, base.width, base.height);
    const canvas = ss.annotCanvas || el('annot-canvas');
    if (canvas) annot.updateCropData(null, canvas.width, canvas.height);
    const meta = window.__blinkScreenMeta || {};
    const point = cssPointToScreen(ss.selCss.x, ss.selCss.y, meta), size = cssRectToBitmap(ss.selCss, meta);
    state.regionPhys = {...point, w: size.w, h: size.h};
    syncRegionVisual(); el('gif-region-frame')?.classList.remove('hidden');
    setDesktopMode(true);
    return preparation.open().then(id => {
        if (!state.owns(epoch, null)) return;
        state.preparationId = id;
        setDesktopMode(state.desktopMode, {select: false});
    });
}

export function restorePreparationAnnotations() {
    if (!ss.annotCanvas) return;
    const source = ss.screenshotOffscreen || ss.screenshot;
    if (!source || !ss.selCss) return;
    const bmp = cssRectToBitmap(ss.selCss, window.__blinkScreenMeta || {});
    try {
        const canvas = document.createElement('canvas');canvas.width=bmp.w;canvas.height=bmp.h;
        const ctx=canvas.getContext('2d');
        ctx.drawImage(source,bmp.x,bmp.y,bmp.w,bmp.h,0,0,bmp.w,bmp.h);
        annot.updateCropData(ctx.getImageData(0,0,bmp.w,bmp.h),bmp.w,bmp.h);
    } catch (error) { console.error('[recording] preparation annotation restore failed', error); }
}

/** 录制会话结束（停止/取消由后端收场并隐藏 overlay）——仅复位 UI 投影。 */
function endOverlaySession() {
    preparationSelection?.cancel();
    void preparation.close().catch(error => console.error('[recording] preparation cleanup failed', error));
    regionDrag = null;
    annotationUploader?.stop(); annotationUploader = null;
    annot.setRenderListener?.(null);
    clearInterval(state.tickTimer); clearInterval(state.hitRectTimer);
    state.reset();
    endAudioRecording();
    lastStatusWarning = '';
    delete ss.gifRecHooks;
    ss._closeToolDropdowns?.();
    el('sub-panel')?.classList.add('hidden');
    closeRecordingPanel(); showRecordingMessage('');
    el('gif-region-frame')?.classList.add('hidden');
    ss.sent = false;
    renderRecordingUi();
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
    if (p.revision && p.revision <= state.snapshotRevision) return;
    if (p.revision) state.snapshotRevision = p.revision;
    if (p.phase === 'idle') {
        endOverlaySession();
        return;
    }
    // 会话已收场（正常/取消/异常）——UI 冻结在收尾态，等 overlay 隐藏或
    // 下一轮截图会话 reset。
    if (p.phase === 'done') {
        // 后端已收场/即将隐藏 overlay；停止响应输入
        state.setMode('stopping');
        renderRecState();
        return;
    }
    if (p.phase === 'stopping' || state.stopping) {
        state.setMode('stopping');
        renderRecState();
        return;
    }
    state.setMode(['waiting', 'countdown'].includes(p.phase) ? 'starting' : p.phase);
    state.audioError = p.audio?.error;
    applyAudioStatus(p.audio, state.sessionId);
    state.elapsedBase = p.elapsedMs ?? state.elapsedBase;
    state.elapsedAt = performance.now();
    state.frames = p.frames ?? state.frames;
    state.storage = p.storage;state.targetFps = p.targetFps;state.actualFps = p.actualFps;
    renderRecState();
}

function renderRecState() {
    renderRecordingUi();
    const time=el('gif-rec-time');
    if(time) time.textContent=formatTimecode(currentElapsedMs()).replace(/\.\d+$/, '');
    const label=t('gif.rec.' + (state.storage?.pauseReason === 'low_space' ? 'spaceLow' : state.storage?.pauseReason === 'space_unavailable' ? 'spaceQueryFailed' : 'paused'));
    const warning=state.storage?.pauseReason ? t('gif.rec.resourcePaused',{reason:label}) : state.audioError || '';
    if(warning || lastStatusWarning) showRecordingMessage(warning);
    lastStatusWarning=warning;
}

function currentElapsedMs() {
    if (state.mode !== 'recording') return state.elapsedBase;
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
        if (timeEl && state.mode === 'recording') {
            timeEl.textContent = formatTimecode(currentElapsedMs()).replace(/\.\d+$/, '');
        }
    }, TICK_INTERVAL_MS);
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
    if (state.preparing) for (const handle of document.querySelectorAll('#gif-region-frame [data-recording-resize], #gif-region-frame [data-recording-edge]')) push(handle);
    for (const panel of document.querySelectorAll('.recording-panel:not(.hidden)')) push(panel);
    // 打开的下拉（data-open="true"）与浮层
    for (const dom of document.querySelectorAll('#toolbar .dropdown[data-open="true"]')) push(dom);
    push(el('ss-action-menu'));
    push(el('sub-panel'));
    return rects;
}

/** 切换输入模式并上报；操作桌面模式启动命中矩形周期上报。 */
function setDesktopMode(desktop, {select = true} = {}) {
    if (!state.visible || state.stopping || (!desktop && state.restoredControlsOnly)) return;
    retainToolbarAnchor();
    state.drawing = !desktop;
    ss._closeToolDropdowns?.();
    el('sub-panel')?.classList.add('hidden');
    renderRecordingUi();
    if(select && (!state.active || (!desktop && !REC_TOOLS.has(annot.getTool())))) selectTool(desktop ? 'select' : 'pencil');
    clearInterval(state.hitRectTimer); state.hitRectTimer=0;
    if(!state.active && state.preparationId == null) return;
    const sessionId=state.sessionId,epoch=state.epoch;
    const applied = overlayInput('gif_overlay_input',{desktop,rects:collectHitRects()}).catch(error=>{
        if(state.owns(epoch,sessionId)) console.error('[recording] input mode failed',error);
    });
    if(desktop) state.hitRectTimer=setInterval(()=>{
        if(!state.owns(epoch,sessionId)||!state.desktopMode) return;
        overlayInput('gif_overlay_hit_rects',{rects:collectHitRects()}).catch(()=>{});
    },HIT_RECT_INTERVAL_MS);
    return applied;
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
        const meta = window.__blinkScreenMeta || {};
        const {scaleX, scaleY} = getRenderScale(meta);
        const geometry = recordingFrameGeometry(state.regionPhys, meta, scaleX, scaleY);
        Object.assign(frame.style, geometry);
        const size = el('gif-prep-size');
        if (size && state.regionPhys) {
            size.textContent = `${state.regionPhys.w} × ${state.regionPhys.h}`;
            size.style.top = ss.selCss.y < 24 ? '4px' : '-24px';
        }
    }
}

function changePreparationSelection(rect, commit) {
    if (!state.preparing || state.active) return;
    ss.selCss = {...rect};
    const meta = window.__blinkScreenMeta || {};
    const point = cssPointToScreen(rect.x, rect.y, meta), size = cssRectToBitmap(rect, meta);
    state.regionPhys = {...point, w: size.w, h: size.h};
    if (ss.annotCanvas) {
        if (commit) {
            Object.assign(ss.annotCanvas.style, {width: `${rect.w}px`, height: `${rect.h}px`});
            ss.annotCanvas.width = size.w; ss.annotCanvas.height = size.h;
            annot.updateCropData(null, size.w, size.h);
        }
    }
    syncRegionVisual();
    if (commit) {
        ss._syncRecordingToolbar?.();
    }
}

/** 后端对账（含拖动钳制结果与新录制会话初始区域）。 */
function applyRegionUpdated(p) {
    if (p.sessionId !== state.sessionId) return;
    if (regionDrag) return; // 拖动时以本地最新位置为准，队列排空后再对账。
    if (typeof p.x !== 'number' || typeof p.w !== 'number') return;
    state.regionPhys = {x: p.x, y: p.y, w: p.w, h: p.h};
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    ss.selCss = screenRectToCss({x: p.x, y: p.y, w: p.w, h: p.h}, meta);
    syncRegionVisual();
}

/** 同一位移同时作用于选区和工具栏，边缘钳制也只算一次。 */
export function recordingDragDelta(region, dx, dy, viewport) {
    return {dx:Math.max(-region.x,Math.min(dx,Math.max(0,viewport.w-region.w)-region.x)),
        dy:Math.max(-region.y,Math.min(dy,Math.max(0,viewport.h-region.h)-region.y))};
}

function beginRecordingGripDrag() {
    if(!state.visible || state.busy || !ss.selCss) return false;
    const origin={...ss.selCss},epoch=state.epoch,sessionId=state.sessionId;
    const meta=window.__blinkScreenMeta || {};
    const current=()=>regionDrag===drag && state.owns(epoch,sessionId);
    let pending=null,task=null;
    const drain=()=>{
        if(task) return task;
        task=(async()=>{
            while(pending && current()) {
                const point=pending;pending=null;
                await invoke('gif_move_recording_region',{...point,sessionId});
            }
        })().catch(error=>{if(current()) showRecordingMessage(normalizeError(error).message);})
            .finally(()=>{task=null;});
        return task;
    };
    const drag={
        move(dx,dy) {
            if(!current()) return null;
            const delta=recordingDragDelta(origin,dx,dy,{w:window.innerWidth,h:window.innerHeight});
            ss.selCss={...origin,x:origin.x+delta.dx,y:origin.y+delta.dy};
            if(state.active) {
                const point=cssPointToScreen(ss.selCss.x,ss.selCss.y,meta);
                state.regionPhys={...state.regionPhys,...point};
                pending=point;void drain();syncRegionVisual();
            } else {
                updateLiveSelection(ss.selCss);
                if(ss.annotCanvas) {ss.annotCanvas.style.left=ss.selCss.x+'px';ss.annotCanvas.style.top=ss.selCss.y+'px';}
                const point=cssPointToScreen(ss.selCss.x,ss.selCss.y,meta),size=cssRectToBitmap(ss.selCss,meta);
                state.regionPhys={...point,w:size.w,h:size.h};syncRegionVisual();
            }
            return delta;
        },
        async end() {
            if(!current()) return;
            if(state.active) {
                await task;
                if(!current()) return;
                regionDrag=null;
                setDesktopMode(state.desktopMode,{select:false});
                const snapshot=await invoke('gif_recorder_state').catch(()=>null);
                if(state.owns(epoch,sessionId) && !regionDrag && snapshot?.region) applyRegionUpdated({sessionId:snapshot.sessionId,...snapshot.region});
            } else {
                regionDrag=null;
                if(state.preparationId != null) setDesktopMode(state.desktopMode,{select:false});
                drawFinalSelection();
            }
        },
    };
    regionDrag=drag;
    closeRecordingPanel();ss._closeToolDropdowns?.();
    if(!state.active) ss._invalidateSelectionContent?.({preservePresentation:true});
    if(state.active || state.preparationId != null) void overlayInput('gif_overlay_input',{desktop:false,rects:collectHitRects()}).catch(()=>{});
    return drag;
}

// ── 停止 / 取消（受保护） / 暂停 ────────────────────────────────────────

async function stopRecording() {
    if (!state.active || state.stopping) return;
    closeRecordingPanel();
    const epoch=state.epoch,sessionId=state.sessionId,previousMode=state.mode;
    state.setMode('stopping');
    renderRecState();
    try {
        await invoke('gif_stop_recording', {sessionId});
    } catch (err) {
        if(!state.owns(epoch,sessionId)) return;
        state.setMode(previousMode);
        console.error('[screenshot] 停止录制失败', err);
        showTransientHint(`停止失败：${normalizeError(err).message}`, {isError: true, duration: 3000});
        renderRecState();
    }
}

/** 丢弃必须经过明确确认；关闭面板不会结束录制。 */
function resetCancel() {
    state.cancelArmed = false;
}
function cancelRecording() {
    if (state.stopping) return;
    const panel = el('gif-discard-confirm'), anchor = el('gif-rec-stop');
    if (!panel || !anchor) return;
    state.cancelArmed = openRecordingPanel(panel, anchor, resetCancel);
    if (state.cancelArmed) el('gif-discard-keep')?.focus({preventScroll: true});
}
async function confirmDiscard() {
    if (!state.active || state.stopping || !state.cancelArmed) return;
    const epoch=state.epoch,sessionId=state.sessionId,previousMode=state.mode;
    closeRecordingPanel();
    state.setMode('stopping');
    renderRecState();
    try { await invoke('gif_cancel_recording', {sessionId}); }
    catch (error) {
        if(!state.owns(epoch,sessionId)) return;
        state.setMode(previousMode);
        console.error('[screenshot] 取消录制失败', error);
        renderRecState();
        showRecordingMessage(`丢弃失败：${normalizeError(error).message}`);
    }
}

/** 物理像素外扩后再换算，border-box 的四条边都完整位于采集矩形外。 */
export function recordingFrameGeometry(region, meta = {}, scaleX = 1, scaleY = 1) {
    const display = meta.physicalDisplays?.find(d => region.x >= d.x && region.x < d.x + d.w && region.y >= d.y && region.y < d.y + d.h);
    const border = Math.max(1, Math.round((display?.dpi || 96) / 96));
    return {
        left: `${(region.x - (meta.vx || 0) - border) / scaleX}px`,
        top: `${(region.y - (meta.vy || 0) - border) / scaleY}px`,
        width: `${(region.w + border * 2) / scaleX}px`,
        height: `${(region.h + border * 2) / scaleY}px`,
        borderLeftWidth: `${border / scaleX}px`, borderRightWidth: `${border / scaleX}px`,
        borderTopWidth: `${border / scaleY}px`, borderBottomWidth: `${border / scaleY}px`,
    };
}

async function togglePause() {
    if (state.stopping) return;
    const sessionId = state.sessionId, paused = state.mode === 'paused';
    try {
        if (paused) {
            await closeAudioSettings();
            await annotationUploader?.flush();
            await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
        }
        if (!state.active || state.sessionId !== sessionId || state.stopping) return;
        await invoke(paused ? 'gif_resume_recording' : 'gif_pause_recording', {sessionId});
    } catch (error) {
        console.error('[screenshot] 暂停/继续失败', error);
        if (state.active && state.sessionId === sessionId) showRecordingMessage(`操作失败：${normalizeError(error).message}`);
    }
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
            await annotationUploader?.flush();
            if (current()) await invoke('gif_resume_recording', {sessionId});
        }
    };
    try {
        if (!current()) throw new Error('录制会话已结束');
        if (textOverlapsRegion(pos, ss.selCss, size)) {
            let snapshot = await invoke('gif_recorder_state');
            if (snapshot?.sessionId !== sessionId || !current()) throw new Error('录制会话已切换');
            if (snapshot.phase === 'recording') {
                await invoke('gif_pause_recording', {sessionId: state.sessionId});
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
    const frame = el('gif-region-frame');
    if (frame && !preparationSelection) preparationSelection = bindRecordingSelection({frame, state, ss,
        onChange: changePreparationSelection,
        onInvalidate: () => ss._invalidateSelectionContent?.({preservePresentation: true}),
        onInteraction(active, epoch) {
            closeRecordingPanel();
            selectionInputTask = selectionInputTask.catch(() => {}).then(async () => {
                await waitForRecordingPreparation();
                if (!state.owns(epoch, null) || !state.preparing) return;
                return active ? overlayInput('gif_overlay_input', {desktop: false, rects: collectHitRects()}) :
                    setDesktopMode(state.desktopMode, {select: false});
            }).catch(error => { if (state.owns(epoch, null)) console.error('[recording] selection input failed', error); });
        },
    });
    el('gif-rec-draw')?.addEventListener('click',()=>{
        if(state.visible && !state.busy) setDesktopMode(state.drawing);
    });
    el('gif-discard-yes')?.addEventListener('click', confirmDiscard);
    el('gif-discard-keep')?.addEventListener('click', () => closeRecordingPanel({focus: true}));
    setRecordingUiListener(() => {
        if (state.visible && state.desktopMode && (state.active || state.preparationId != null)) overlayInput('gif_overlay_hit_rects', {rects: collectHitRects()}).catch(() => {});
    });
    if (typeof MutationObserver === 'function') {
        const observer = new MutationObserver(() => { if (state.active) reflowRecordingPanels(); });
        for (const node of [...document.querySelectorAll('#toolbar .dropdown'), el('sub-panel')]) {
            if (node) observer.observe(node, {attributes: true, attributeFilter: ['data-open', 'class']});
        }
    }
    el('gif-record-toggle')?.addEventListener('click',()=>{if(state.active&&!state.busy) void togglePause();});
    el('gif-rec-stop')?.addEventListener('click', stopRecording);
    el('gif-rec-discard')?.addEventListener('click',()=>{if(state.active&&!state.busy) cancelRecording();});
    // Esc 由 index.js 的分层链先处理输入框/弹层，再在录制态消费。
}

/** 会话外部收场兜底（overlay 被 hide_screenshot_overlay 隐藏后调用）。 */
export function handleOverlayHidden() {
    // 准备态与录制态共用同一清理路径，工具从未离开宿主，不再需要 DOM 归还。
    endOverlaySession();
}
