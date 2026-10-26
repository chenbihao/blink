//! GIF 编辑器（0.28.7，gif-editor 窗口）。
//!
//! 结构：
//! - 状态真源在后端（domain Timeline）；本页持视图快照 + 素材帧图缓存。
//! - **时间轴**：ScreenToGif 式全量连续缩略图——虚拟化渲染（只建可视区
//!   ±缓冲帧的 DOM，总宽按帧数撑起），滚动到视口时批量懒解码
//!   （`gif_get_frames` ≤24 帧/批、串行队列防并发），无分页。
//! - **预览播放**：setTimeout 链按帧 `durMs` 调度；空格启动先等当前帧
//!   剩余时长（不丢帧节奏）；播放头前方预取窗口（+24 帧）后台解码，
//!   缓存 miss 时保持上一帧画面不闪；播放头实时同步时间轴高亮并滚动跟随。
//! - **预览清晰度**：解码宽度 = 预览显示区物理像素（clamp 到源宽），不在
//!   显示时拉伸；预览缓存按字节预算淘汰。
//! - 选中模型：`selectedSet`——单击单选、Ctrl 切换、Shift 范围；Delete/右键
//!   删除（单帧 `delete_frame`、多帧 `delete_frames` 批量单 undo）。
//!
//! 键盘：Space 播放/暂停；←/→ 帧步进；Delete/Backspace 删除选中。

import {ensureSpriteLoaded} from '../shared/icon.js';
import {invoke, listen, normalizeError, saveDialog} from '../shared/tauri.js';
import {commands, formatFrameDur, formatSeconds, frameState, nextPlayStep, playToSrc, srcToPlayIndex} from './timeline-view.js';

ensureSpriteLoaded();

/** 缩略图固定高度（CSS px）。 */
const THUMB_H = 64;
const THUMB_GAP = 6;
/** 可视区外仍保留的缓冲帧数（虚拟化窗口）。 */
const VIRTUAL_BUFFER = 10;
/** 播放头前方预取帧数。 */
const PLAY_PREFETCH = 24;
/** 预览缓存字节预算（ImageBitmap 按源像素估算）。 */
const PREVIEW_BYTES_BUDGET = 96 << 20;

const $ = id => document.getElementById(id);
const els = {
    title: $('session-title'),
    meta: $('session-meta'),
    canvas: $('preview-canvas'),
    canvasEmpty: $('preview-empty'),
    pos: $('preview-pos'),
    dur: $('preview-dur'),
    totalDur: $('total-dur'),
    strip: $('frame-strip'),
    stripScroll: $('timeline-scroll'),
    frameCountInd: $('frame-count-ind'),
    btnPlay: $('btn-play'),
    speed: $('sel-speed'),
    hold: $('sel-hold'),
    loop: $('sel-loop'),
    undo: $('btn-undo'),
    redo: $('btn-redo'),
    exportBtn: $('btn-export'),
    dialog: $('export-dialog'),
    exportPath: $('export-path'),
    pickPath: $('btn-pick-path'),
    exportConfirm: $('btn-export-confirm'),
    exportCancelReq: $('btn-export-cancel-req'),
    exportClose: $('btn-export-close'),
    progressRow: $('export-progress-row'),
    progress: $('export-progress'),
    progressText: $('export-progress-text'),
};

/** 会话状态。 */
const state = {
    sessionId: null,
    view: null,
    playIdx: 0,
    selectedSet: new Set(),
    anchor: null,
    playing: false,
    playTimer: 0,
    /** 当前帧开始展示的时刻（空格启动时据此保留剩余时长）。 */
    frameShownAt: 0,
    /** 时间轴几何（会话加载时按宽高比计算）。 */
    thumbW: 96,
    slotW: 102,
    /** 预览解码宽度（显示区物理像素，clamp 到源宽）。 */
    previewWidth: 480,
    thumbCache: new Map(),   // srcIdx -> ImageBitmap（缩略 160 档）
    previewCache: new Map(), // srcIdx -> ImageBitmap（previewWidth 档）
    previewBytes: 0,
    /** 已建 DOM 的帧槽（虚拟化窗口内）。 */
    liveThumbs: new Map(),   // srcIdx -> HTMLElement
    /** 解码批次队列（串行执行，防并发 invoke 爆炸）。 */
    decoding: false,
    pendingBatches: [],
    exporting: false,
};

// ── 帧解码（批量懒加载 + 缓存淘汰） ─────────────────────────────────────

function requestDecode(sessionId, indices, maxW, cache, priority = false) {
    if (!indices.length) return;
    const batch = {sessionId, indices, maxW, cache};
    // priority（播放预取/当前预览）插队队首——后台顺序预载让位于交互
    if (priority) state.pendingBatches.unshift(batch);
    else state.pendingBatches.push(batch);
    pumpDecode();
}

async function pumpDecode() {
    if (state.decoding) return;
    state.decoding = true;
    while (state.pendingBatches.length) {
        const {sessionId, indices, maxW, cache} = state.pendingBatches.shift();
        const missing = indices.filter(i => !cache.has(i) && i < (state.view?.frameCount ?? 0));
        if (!missing.length || sessionId !== state.sessionId) continue;
        for (let i = 0; i < missing.length; i += 24) {
            const batch = missing.slice(i, i + 24);
            try {
                const frames = await invoke('gif_get_frames', {sessionId, indices: batch, maxW});
                for (const f of frames) {
                    const blob = await (await fetch(`data:image/png;base64,${f.pngBase64}`)).blob();
                    cache.set(f.index, await createImageBitmap(blob));
                    if (cache === state.previewCache) {
                        state.previewBytes += f.width * f.height * 4;
                    }
                }
            } catch (err) {
                console.error('[gif-editor] 帧解码失败', err);
            }
            if (sessionId !== state.sessionId) break; // 会话已切换：丢弃过期批次
        }
        evictPreviewCache();
        refreshLiveThumbs();
    }
    state.decoding = false;
}

function evictPreviewCache() {
    // thumb 兜底上限（450 帧 160px ≈ 26MB，一般全存；极端长会话裁剪）
    let thumbBytes = 0;
    for (const [, bmp] of state.thumbCache) thumbBytes += bmp.width * bmp.height * 4;
    while (state.thumbCache.size && thumbBytes > 64 << 20) {
        const firstKey = state.thumbCache.keys().next().value;
        const bmp = state.thumbCache.get(firstKey);
        thumbBytes -= bmp.width * bmp.height * 4;
        state.thumbCache.delete(firstKey);
    }
    let bytes = state.previewBytes;
    // FIFO 淘汰到预算内（Map 插入序 ≈ 使用序；播放预取窗口天然滚动）
    for (const [idx, bmp] of state.previewCache) {
        if (bytes <= PREVIEW_BYTES_BUDGET) break;
        const size = bmp.width * bmp.height * 4;
        state.previewCache.delete(idx);
        bytes -= size;
    }
    state.previewBytes = bytes;
}

// ── 时间轴（虚拟化连续渲染） ────────────────────────────────────────────

function layoutGeometry(view) {
    const aspect = (view.meta?.width || 16) / (view.meta?.height || 9);
    state.thumbW = Math.max(48, Math.round(THUMB_H * aspect));
    state.slotW = state.thumbW + THUMB_GAP;
    els.strip.style.width = `${view.frameCount * state.slotW}px`;
    els.frameCountInd.textContent = `${view.frameCount} 帧`;
}

function visibleRange() {
    const sc = els.stripScroll;
    const start = Math.max(0, Math.floor(sc.scrollLeft / state.slotW) - VIRTUAL_BUFFER);
    const end = Math.min(
        state.view.frameCount - 1,
        Math.ceil((sc.scrollLeft + sc.clientWidth) / state.slotW) + VIRTUAL_BUFFER,
    );
    return [start, end];
}

function relayoutStrip() {
    const view = state.view;
    if (!view) return;
    const [start, end] = visibleRange();
    for (const [idx, el] of state.liveThumbs) {
        if (idx < start || idx > end) {
            el.remove();
            state.liveThumbs.delete(idx);
        }
    }
    const missingThumbs = [];
    for (let idx = start; idx <= end; idx++) {
        if (!state.liveThumbs.has(idx)) {
            const el = buildThumb(view, idx);
            els.strip.appendChild(el);
            state.liveThumbs.set(idx, el);
        }
        if (!state.thumbCache.has(idx)) missingThumbs.push(idx);
    }
    if (missingThumbs.length) {
        requestDecode(state.sessionId, missingThumbs, 160, state.thumbCache);
    }
}

/** 已建槽但当时缺图的，解码完成后回填。 */
function refreshLiveThumbs() {
    for (const [, el] of state.liveThumbs) {
        const img = el.querySelector('img');
        if (img && !img.src) {
            const idx = Number(el.dataset.idx);
            const bmp = state.thumbCache.get(idx);
            if (bmp) img.src = bmpToDataUrl(bmp);
        }
    }
}

let scrollRaf = 0;
els.stripScroll.addEventListener('scroll', () => {
    if (scrollRaf) return;
    scrollRaf = requestAnimationFrame(() => {
        scrollRaf = 0;
        relayoutStrip();
    });
});

function buildThumb(view, idx) {
    const fs = frameState(view, idx);
    const btn = document.createElement('button');
    btn.className = 'frame-thumb';
    btn.dataset.idx = String(idx);
    btn.style.left = `${idx * state.slotW}px`;
    btn.style.width = `${state.thumbW}px`;
    if (!fs.visible) btn.classList.add('out-of-range');
    if (state.selectedSet.has(idx)) btn.classList.add('selected');
    const img = document.createElement('img');
    img.style.height = `${THUMB_H}px`;
    img.style.width = '100%';
    img.style.objectFit = 'contain';
    const bmp = state.thumbCache.get(idx);
    if (bmp) img.src = bmpToDataUrl(bmp);
    btn.appendChild(img);
    const tag = document.createElement('span');
    tag.className = 'frame-tag';
    tag.textContent = formatFrameDur(rawFrameDur(view, idx));
    btn.appendChild(tag);
    if (fs.held) {
        const hold = document.createElement('span');
        hold.className = 'hold-tag';
        hold.textContent = 'H';
        btn.appendChild(hold);
    }
    btn.title = fs.deleted ? '已删除（撤销可恢复）'
        : !fs.inTrim ? '修剪范围外'
        : fs.held ? `帧 ${idx} · 停留 ${formatFrameDur(view.holds[String(idx)])}`
        : `帧 ${idx}`;
    btn.addEventListener('click', ev => handleThumbClick(idx, ev));
    btn.addEventListener('contextmenu', ev => handleThumbContextMenu(idx, ev));
    return btn;
}

/** 素材原始帧时长（已删/修剪外帧用默认 1/fps 近似展示）。 */
function rawFrameDur(view, srcIdx) {
    const fps = view.meta?.fps || 15;
    const f = (view.frames || []).find(x => x.srcIdx === srcIdx);
    return f ? f.durMs : Math.round(1000 / fps);
}

function bmpToDataUrl(bmp) {
    const c = document.createElement('canvas');
    c.width = bmp.width;
    c.height = bmp.height;
    c.getContext('2d').drawImage(bmp, 0, 0);
    return c.toDataURL('image/png');
}

/** 更新全部已建槽的选中态（选择集变化后）。 */
function refreshSelectionClasses() {
    for (const [idx, el] of state.liveThumbs) {
        el.classList.toggle('selected', state.selectedSet.has(idx));
    }
}

// ── 预览 ─────────────────────────────────────────────────────────────────

function currentPlaySrc() {
    return playToSrc(state.view, state.playIdx);
}

function renderPreview() {
    const view = state.view;
    if (!view || !view.frames.length) {
        els.canvasEmpty.classList.remove('hidden');
        return;
    }
    const cur = Math.min(state.playIdx, view.frames.length - 1);
    const srcIdx = view.frames[cur].srcIdx;
    const bmp = state.previewCache.get(srcIdx);
    if (bmp) {
        drawPreview(bmp);
    } else {
        // miss：保持上一帧画面（不闪不黑），请求解码；完成后若播放头未变再上屏
        requestDecode(state.sessionId, [srcIdx], state.previewWidth, state.previewCache, true);
        const wait = setInterval(() => {
            const b = state.previewCache.get(srcIdx);
            if (b || currentPlaySrc() !== srcIdx || !state.view) {
                clearInterval(wait);
                if (b && currentPlaySrc() === srcIdx) drawPreview(b);
            }
        }, 30);
    }
    els.pos.textContent = `${cur + 1} / ${view.frames.length}`;
    els.dur.textContent = formatFrameDur(view.frames[cur].durMs) + (view.frames[cur].held ? '（停留）' : '');
    els.totalDur.textContent = `总时长 ${formatSeconds(view.totalMs)}`;
}

function drawPreview(bmp) {
    const c = els.canvas;
    if (c.width !== bmp.width) c.width = bmp.width;
    if (c.height !== bmp.height) c.height = bmp.height;
    c.getContext('2d').drawImage(bmp, 0, 0);
    els.canvasEmpty.classList.add('hidden');
}

/** 预览解码宽度：显示区物理像素（clamp 240..源宽）——显示不拉伸，不模糊。 */
function measurePreviewWidth(view) {
    const stage = els.canvas.parentElement;
    const dpr = window.devicePixelRatio || 1;
    const css = stage?.clientWidth || 640;
    const physical = Math.round(css * dpr);
    state.previewWidth = Math.max(240, Math.min(physical, view.meta?.width || physical));
}

// ── 播放（受控时钟 + 预取） ──────────────────────────────────────────────

function scheduleNext(delayMs) {
    clearTimeout(state.playTimer);
    state.playTimer = setTimeout(playTick, Math.max(0, delayMs));
}

function playTick() {
    if (!state.playing) return;
    const view = state.view;
    if (!view || !view.frames.length) return;
    const step = nextPlayStep(view.frames, state.playIdx);
    state.playIdx = step.cursor;
    state.frameShownAt = performance.now();
    renderPreview();
    prefetchAhead();
    syncPlayhead(playToSrc(view, step.cursor));
    scheduleNext(step.durMs);
}

/** 播放头前方预取（预览档位）——播放中后台解码，避免到帧才解码的卡顿。 */
function prefetchAhead() {
    const view = state.view;
    if (!view) return;
    const need = [];
    for (let i = 1; i <= PLAY_PREFETCH && state.playIdx + i < view.frames.length; i++) {
        const src = view.frames[state.playIdx + i].srcIdx;
        if (!state.previewCache.has(src)) need.push(src);
    }
    if (need.length) requestDecode(state.sessionId, need, state.previewWidth, state.previewCache);
}

function setPlaying(on) {
    state.playing = on;
    const icons = els.btnPlay.querySelectorAll('.icon');
    if (icons.length === 2) {
        icons[0].classList.toggle('hidden', on);
        icons[1].classList.toggle('hidden', !on);
    }
    clearTimeout(state.playTimer);
    if (on) {
        // 保留当前帧剩余展示时长（不立即跳帧——空格启动也丝滑）
        const view = state.view;
        if (view && view.frames.length) {
            const cur = Math.min(state.playIdx, view.frames.length - 1);
            const elapsed = performance.now() - state.frameShownAt;
            scheduleNext(view.frames[cur].durMs - elapsed);
        }
    }
}

/** 播放头轻量同步：只切高亮类 + 滚动跟随（连续时间轴，无翻页）。 */
function syncPlayhead(srcIdx) {
    if (srcIdx == null) return;
    state.selectedSet = new Set([srcIdx]);
    const thumb = state.liveThumbs.get(srcIdx);
    if (thumb) {
        refreshSelectionClasses();
        thumb.scrollIntoView({block: 'nearest', inline: 'nearest', behavior: 'auto'});
    } else {
        // 不在虚拟化窗口：滚到目标位置（scroll 事件触发 relayout 建槽）
        els.stripScroll.scrollLeft = Math.max(0, srcIdx * state.slotW - els.stripScroll.clientWidth / 2);
        requestAnimationFrame(() => {
            refreshSelectionClasses();
            state.liveThumbs.get(srcIdx)
                ?.scrollIntoView({block: 'nearest', inline: 'nearest'});
        });
    }
}

// ── 选中模型 ─────────────────────────────────────────────────────────────

function handleThumbClick(idx, ev) {
    if (ev.shiftKey && state.anchor != null) {
        const [a, b] = state.anchor < idx ? [state.anchor, idx] : [idx, state.anchor];
        state.selectedSet = new Set();
        for (let i = a; i <= b; i++) state.selectedSet.add(i);
    } else if (ev.ctrlKey || ev.metaKey) {
        state.anchor = idx;
        if (state.selectedSet.has(idx)) state.selectedSet.delete(idx);
        else state.selectedSet.add(idx);
    } else {
        state.selectedSet = new Set([idx]);
        state.anchor = idx;
        const play = srcToPlayIndex(state.view, idx);
        if (play >= 0) state.playIdx = play;
        state.frameShownAt = performance.now();
    }
    refreshSelectionClasses();
    renderPreview();
    renderControls();
}

function handleThumbContextMenu(idx, ev) {
    ev.preventDefault();
    if (!state.selectedSet.has(idx)) {
        state.selectedSet = new Set([idx]);
    }
    deleteSelected();
}

async function deleteSelected() {
    const frames = [...state.selectedSet].filter(i => i < (state.view?.frameCount ?? 0));
    if (!frames.length) return;
    const cmd = frames.length === 1
        ? commands.deleteFrame(frames[0])
        : commands.deleteFrames(frames);
    state.selectedSet.clear();
    state.anchor = null;
    await edit(cmd);
}

// ── 会话加载 ─────────────────────────────────────────────────────────────

async function loadSession(sessionId) {
    if (!sessionId) return;
    state.sessionId = sessionId;
    state.playIdx = 0;
    state.selectedSet.clear();
    state.anchor = null;
    state.playing = false;
    clearTimeout(state.playTimer);
    state.thumbCache.clear();
    state.previewCache.clear();
    state.previewBytes = 0;
    state.pendingBatches.length = 0;
    for (const [, el] of state.liveThumbs) el.remove();
    state.liveThumbs.clear();
    els.stripScroll.scrollLeft = 0;
    state.frameShownAt = performance.now();
    state.view = await invoke('gif_get_session', {sessionId});
    const m = state.view.meta;
    els.title.textContent = 'GIF 编辑器';
    els.meta.textContent = `${m.width}×${m.height} · ${m.fps}fps · ${state.view.frameCount} 帧`;
    measurePreviewWidth(state.view);
    layoutGeometry(state.view);
    relayoutStrip();
    renderPreview();
    renderControls();
    startPreload();
}

/** 打开即静默预载：缩略全量 + 预览按序（后台批次，播放/交互优先插队）。
 *  编辑/播放大多数情况零解码等待；预览缓存仍受字节预算约束滚动淘汰。 */
function startPreload() {
    const view = state.view;
    if (!view) return;
    // 缩略全量（160px，450 帧 ≈ 26MB——时间轴滚动零延迟）
    const allThumbs = [];
    for (let i = 0; i < view.frameCount; i++) {
        if (!state.thumbCache.has(i)) allThumbs.push(i);
    }
    if (allThumbs.length) requestDecode(state.sessionId, allThumbs, 160, state.thumbCache);
    // 预览顺序预载（从播放头开始；预算内尽可能多）
    const seq = [];
    for (let i = state.playIdx; i < view.frames.length; i++) {
        const src = view.frames[i].srcIdx;
        if (!state.previewCache.has(src)) seq.push(src);
    }
    if (seq.length) requestDecode(state.sessionId, seq, state.previewWidth, state.previewCache);
}

// ── 编辑 ─────────────────────────────────────────────────────────────────

async function edit(command) {
    if (!state.sessionId) return;
    state.view = await invoke('gif_edit', {sessionId: state.sessionId, command});
    if (state.playIdx >= state.view.frames.length) state.playIdx = 0;
    state.frameShownAt = performance.now();
    layoutGeometry(state.view);
    relayoutStrip();
    renderPreview();
    renderControls();
}

function renderControls() {
    const view = state.view;
    if (!view) return;
    els.undo.disabled = !view.canUndo;
    els.redo.disabled = !view.canRedo;
    els.speed.value = String(view.speed);
    els.loop.value = view.loopCount == null ? '' : String(view.loopCount);
    const single = state.selectedSet.size === 1 ? [...state.selectedSet][0] : null;
    if (single != null) {
        const held = view.holds && view.holds[String(single)];
        els.hold.value = held ? String(Math.min(2000, Math.round(held / 500) * 500)) : '0';
    }
}

// ── 事件绑定 ─────────────────────────────────────────────────────────────

$('btn-play').addEventListener('click', () => setPlaying(!state.playing));
$('btn-prev').addEventListener('click', () => stepFrame(-1));
$('btn-next').addEventListener('click', () => stepFrame(1));

function stepFrame(dir) {
    setPlaying(false);
    const len = state.view?.frames.length || 0;
    if (!len) return;
    state.playIdx = (state.playIdx + dir + len) % len;
    state.frameShownAt = performance.now();
    const src = currentPlaySrc();
    if (src != null) {
        state.selectedSet = new Set([src]);
        state.anchor = src;
    }
    renderPreview();
    if (src != null) syncPlayhead(src);
}

document.addEventListener('keydown', ev => {
    const tag = document.activeElement?.tagName;
    if (tag === 'INPUT' || tag === 'SELECT' || tag === 'TEXTAREA') return;
    switch (ev.code) {
        case 'Space':
            ev.preventDefault();
            setPlaying(!state.playing);
            break;
        case 'ArrowLeft':
            ev.preventDefault();
            stepFrame(-1);
            break;
        case 'ArrowRight':
            ev.preventDefault();
            stepFrame(1);
            break;
        case 'Delete':
        case 'Backspace':
            if (state.selectedSet.size && !els.dialog.open) {
                ev.preventDefault();
                deleteSelected();
            }
            break;
        default:
            break;
    }
});

$('btn-trim-start').addEventListener('click', () => {
    const src = currentPlaySrc();
    if (src != null) edit(commands.trimStart(src));
});
$('btn-trim-end').addEventListener('click', () => {
    const src = currentPlaySrc();
    if (src != null) edit(commands.trimEnd(src));
});
$('btn-delete-frame').addEventListener('click', deleteSelected);
els.undo.addEventListener('click', () => edit(commands.undo()));
els.redo.addEventListener('click', () => edit(commands.redo()));
$('btn-reset').addEventListener('click', () => edit({type: 'reset'}));

els.speed.addEventListener('change', () => edit(commands.setSpeed(parseFloat(els.speed.value))));
els.loop.addEventListener('change', () => edit(commands.setLoop(els.loop.value === '' ? null : parseInt(els.loop.value, 10))));
els.hold.addEventListener('change', () => {
    const single = state.selectedSet.size === 1 ? [...state.selectedSet][0] : null;
    if (single != null) edit(commands.setHold(single, parseInt(els.hold.value, 10)));
});

// ── 导出 ─────────────────────────────────────────────────────────────────

els.exportBtn.addEventListener('click', () => {
    els.progressRow.classList.add('hidden');
    els.dialog.showModal();
});
els.pickPath.addEventListener('click', async () => {
    const path = await saveDialog({
        title: '导出 GIF',
        defaultPath: defaultExportName(),
        filters: [{name: 'GIF', extensions: ['gif']}],
    });
    if (path) els.exportPath.value = path;
});
function defaultExportName() {
    const d = new Date();
    const pad = n => String(n).padStart(2, '0');
    return `blink-${d.getFullYear()}${pad(d.getMonth() + 1)}${pad(d.getDate())}-${pad(d.getHours())}${pad(d.getMinutes())}.gif`;
}

els.exportConfirm.addEventListener('click', async () => {
    const path = els.exportPath.value.trim();
    if (!path) {
        els.progressText.textContent = '请先选择目标文件';
        els.progressRow.classList.remove('hidden');
        return;
    }
    if (state.exporting) return;
    state.exporting = true;
    els.progressRow.classList.remove('hidden');
    els.progress.value = 0;
    els.progressText.textContent = '准备调色板…';
    try {
        const stats = await invoke('gif_export', {
            sessionId: state.sessionId,
            path,
            maxWidth: $('export-size').value === '' ? null : parseInt($('export-size').value, 10),
            quality: $('export-quality').value,
        });
        els.progressText.textContent =
            `完成：${stats.width}×${stats.height} · ${stats.writtenFrames} 帧 · ${(stats.bytes / 1024).toFixed(0)} KB · ${(stats.encodeMs / 1000).toFixed(1)}s`;
        els.progress.value = 100;
    } catch (err) {
        els.progressText.textContent = `导出失败：${normalizeError(err).message}`;
    } finally {
        state.exporting = false;
    }
});
els.exportCancelReq.addEventListener('click', () => {
    invoke('gif_export_cancel', {sessionId: state.sessionId}).catch(() => {});
});
els.exportClose.addEventListener('click', () => els.dialog.close());

listen('blink://gif-export-progress', event => {
    const p = event.payload;
    if (!p || p.sessionId !== state.sessionId) return;
    if (p.stage === 'preparing') {
        els.progressRow.classList.remove('hidden');
        els.progress.value = p.framesTotal ? Math.round((p.framesDone / p.framesTotal) * 50) : 0;
        els.progressText.textContent = `准备调色板 ${p.framesDone}/${p.framesTotal}`;
    } else if (p.stage === 'quantizing') {
        els.progressText.textContent = '量化调色板…';
        els.progress.value = 50;
    } else if (p.stage === 'encoding') {
        els.progressRow.classList.remove('hidden');
        els.progress.value = 50 + (p.framesTotal ? Math.round((p.framesDone / p.framesTotal) * 50) : 0);
        els.progressText.textContent = `编码中 ${p.framesDone}/${p.framesTotal}`;
    } else if (p.stage === 'cancelled') {
        els.progressText.textContent = '已取消';
    }
});

listen('blink://gif-editor-session', event => {
    const sid = event.payload?.sessionId;
    if (sid && sid !== state.sessionId) loadSession(sid).catch(err => console.error('[gif-editor] 会话加载失败', err));
});

invoke('gif_editor_current')
    .then(sid => {
        if (sid) return loadSession(sid);
    })
    .catch(err => console.error('[gif-editor] 初始会话拉取失败', normalizeError(err)));
