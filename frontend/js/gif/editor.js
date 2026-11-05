//! 录制编辑器：分页元数据、可见缩略图及播放头邻域的有界图片缓存。
//! 播放/seek 由后端 revision 绑定的时间窗口定位，不持有整段帧表。
//! 连续多选以范围表达，编辑串行提交；关闭前保存位置，最近录制恢复草稿。

import {discardRecording} from './session-discard.js';
import {bindEditorSelects} from './editor-controls.js';
import {applyThemeFromConfig} from '../shared/theme.js';
import {
    createExportController
} from './export-controller.js';
import {
    ensureSpriteLoaded
} from '../shared/icon.js';
import {
    confirmDialog,
    getCurrentWindow,
    invoke,
    listen,
    normalizeError
} from '../shared/tauri.js';
import {
    EVENTS
} from '../shared/event-names.js';
import {
    applyI18nFromConfig,
    onLangChange,
    t
} from '../i18n/index.js';
import {
    commands,
    formatFrameDur,
    formatSeconds
} from './timeline-view.js';
import {
    clampPreviewLeft,
    formatTimecode,
    keyboardStep,
    playbackOffset,
    pointerRatio,
    ratioToTime
} from './seek-bar.js';
import {
    createRecentList
} from './recent-list.js';
import {
    TimelinePages
} from './timeline-pages.js';
import {
    createAudioPlayback
} from './audio-playback.js';
import {PreviewFrames, readyPreview} from './preview-frames.js';

ensureSpriteLoaded();
const syncEditorSelects = bindEditorSelects();
void applyThemeFromConfig();
const audioPlayback = createAudioPlayback({
    onError: error => showHint(t('gif.audio.previewFailed', {
        msg: normalizeError(error).message
    }))
});
window.addEventListener('pagehide', () => audioPlayback.dispose());

/** 缩略图固定高度（CSS px）。 */
const THUMB_H = 64;
const THUMB_GAP = 6;
/** 可视区外仍保留的缓冲帧数（虚拟化窗口）。 */
const VIRTUAL_BUFFER = 10;
/** 播放头前方预取帧数。 */
/** 预览缓存字节预算（ImageBitmap 按源像素估算）。 */
const PREVIEW_BYTES_BUDGET = 96 << 20;
/** 拖动定位中对最新目标请求全尺寸解码的防抖（ms）。 */

const $ = id => document.getElementById(id);
const els = {
    title: $('session-title'),
    meta: $('session-meta'),
    canvas: $('preview-canvas'),
    canvasEmpty: $('preview-empty'),
    pos: $('preview-pos'),
    dur: $('preview-dur'),
    totalDur: $('total-dur'),
    seekTime: $('seek-time'),
    strip: $('frame-strip'),
    stripScroll: $('timeline-scroll'),
    frameCountInd: $('frame-count-ind'),
    btnPlay: $('btn-play'),
    speed: $('sel-speed'),
    /** 帧时长输入（ms）——作用于全部选中帧（set_hold_many 单 undo）。 */
    durInput: $('inp-dur'),
    durClear: $('btn-dur-clear'),
    btnDelete: $('btn-delete-frame'),
    loop: $('sel-loop'),
    undo: $('btn-undo'),
    redo: $('btn-redo'),
    exportBtn: $('btn-export'),
    dialog: $('export-dialog'),
    hint: $('editor-hint'),
    stopBanner: $('stop-reason-banner'),
    stopBannerText: $('stop-reason-text'),
    stopBannerClose: $('stop-reason-close'),
    /** 进度定位条（slider）及其轨道/填充/播放头/悬浮预览。 */
    seekBar: $('play-progress'),
    playProgress: $('play-progress-fill'),
    seekThumb: $('play-progress-thumb'),
    seekPreview: $('seek-preview'),
    seekPreviewImg: $('seek-preview-img'),
    seekPreviewLabel: $('seek-preview-label'),
    /** 视图根与最近录制面（0.28.23；互斥显示）。 */
    editorRoot: $('gif-editor'),
    recentPane: $('recent-pane'),
    btnRecent: $('btn-recent'),
    btnBackEdit: $('btn-back-edit'),
};

/** 会话状态。 */
const state = {
    sessionId: null,
    deleting: false,
    epoch: 0,
    view: null,
    playIdx: 0,
    selectedSet: new Set(),
    anchor: null,
    playing: false,
    previewSound: true,
    playTimer: 0,
    /** 当前帧开始展示的时刻（空格启动时据此保留剩余时长）。 */
    frameShownAt: 0,
    /** 时间轴几何（会话加载时按宽高比计算）。 */
    thumbW: 96,
    slotW: 102,
    /** 预览解码宽度（显示区物理像素，clamp 到源宽）。 */
    previewWidth: 480,
    // 缩略图直接缓存后端返回的 PNG data URL（字符串）——不再转 ImageBitmap
    // 后又 canvas 重编码回 PNG（每个缩略图一次主线程同步编码，几百帧可观）。
    thumbCache: new Map(), // srcIdx -> data URL（160 档缩略）
    previewCache: new Map(), // srcIdx -> ImageBitmap（previewWidth 档）
    previewBytes: 0,
    /** 已建 DOM 的帧槽（虚拟化窗口内）。 */
    liveThumbs: new Map(), // srcIdx -> HTMLElement
    /** 解码批次队列（串行执行，防并发 invoke 爆炸）。 */
    decoding: false,
    pendingBatches: [],
    decodeKeys: new Set(),
    /** 瞬时提示定时器。 */
    hintTimer: 0,
    /** 进度条拖动中（pointer capture 期间）。 */
    seekDragging: false,
    /** 拖动前是否在播放（松手按此恢复）。 */
    seekWasPlaying: false,
    /** 进度条定位后的帧内偏移（暂停态显示/恢复播放用；步进/点选/播放推进归零）。 */
    seekInFrameMs: 0,
    /** 播放位置上报节流句柄（草稿播放头；提交定位/暂停/步进时上报）。 */
    posReportTimer: 0,
    seekPreviewSrc: null,
};

// ── 帧解码（批量懒加载 + 缓存淘汰） ─────────────────────────────────────
const previewFrames = new PreviewFrames({state, invoke, evict: evictPreviewCache,
    onReady() { if (state.current) presentPreview(); }});

function requestDecode(sessionId, indices, maxW, cache, priority = false) {
    if (cache === state.previewCache) { previewFrames.request(indices, priority); return; }
    const epoch = state.epoch;
    const unique = [];
    for (const idx of indices) {
        if (cache.has(idx)) continue;
        const key = decodeKey(epoch, cache, maxW, idx);
        if (state.decodeKeys.has(key)) {
            // 已排队的目标提升优先级；在途目标复用原请求。
            if (priority) {
                const at = state.pendingBatches.findIndex(b => b.epoch === epoch && b.cache === cache &&
                    b.maxW === maxW && b.indices.includes(idx));
                if (at > 0) state.pendingBatches.unshift(...state.pendingBatches.splice(at, 1));
            }
            continue;
        }
        state.decodeKeys.add(key);
        unique.push(idx);
    }
    indices = unique;
    if (!indices.length) return;
    const batches = [];
    for (let i = 0; i < indices.length; i += 24) {
        batches.push({
            sessionId,
            indices: indices.slice(i, i + 24),
            maxW,
            cache,
            epoch: state.epoch
        });
    }
    // priority（播放预取/当前预览）插队队首——后台顺序预载让位于交互
    if (priority) state.pendingBatches.unshift(...batches);
    else state.pendingBatches.push(...batches);
    while (state.pendingBatches.length > 8) {
        const dropped = state.pendingBatches.pop();
        for (const idx of dropped.indices) state.decodeKeys.delete(decodeKey(dropped.epoch, dropped.cache, dropped.maxW,
            idx));
    }
    pumpDecode();
}

function decodeKey(epoch, cache, maxW, idx) {
    return `${epoch}:${cache === state.thumbCache ? 'thumb' : 'preview'}:${maxW}:${idx}`;
}

async function pumpDecode() {
    if (state.decoding || state.playing) return;
    state.decoding = true;
    while (state.pendingBatches.length) {
        if (state.playing) break;
        const {
            sessionId,
            indices,
            maxW,
            cache,
            epoch
        } = state.pendingBatches.shift();
        try {
            const missing = indices.filter(i => !cache.has(i) && i < (state.view?.frameCount ?? 0));
            if (!missing.length || sessionId !== state.sessionId || epoch !== state.epoch || (cache === state.previewCache && maxW !== state.previewWidth)) continue;
            for (let i = 0; i < missing.length; i += 24) {
                const batch = missing.slice(i, i + 24);
                try {
                    const frames = await invoke('gif_get_frames', {
                        sessionId,
                        indices: batch,
                        maxW
                    });
                    if (epoch !== state.epoch || (cache === state.previewCache && maxW !== state.previewWidth)) break;
                    for (const f of frames) {
                        cache.set(f.index, `data:image/png;base64,${f.pngBase64}`);
                        evictPreviewCache();
                    }
                } catch (err) {
                    console.error('[gif-editor] 帧解码失败', err);
                }
                if (epoch !== state.epoch) break;
            }
            evictPreviewCache();
            refreshLiveThumbs();
        } finally {
            for (const idx of indices) state.decodeKeys.delete(decodeKey(epoch, cache, maxW, idx));
        }
    }
    state.decoding = false;
}

function evictPreviewCache() {
    // thumb 兜底上限（按 data URL 字符串长度估算字节；一般全存，极端长会话裁剪）
    let thumbBytes = 0;
    for (const [, url] of state.thumbCache) thumbBytes += url.length * 2;
    while (state.thumbCache.size && thumbBytes > 64 << 20) {
        const firstKey = state.thumbCache.keys().next().value;
        const url = state.thumbCache.get(firstKey);
        thumbBytes -= url.length * 2;
        state.thumbCache.delete(firstKey);
    }
    let bytes = state.previewBytes;
    // FIFO 淘汰到预算内（Map 插入序 ≈ 使用序；播放预取窗口天然滚动）
    for (const [idx, bmp] of state.previewCache) {
        if (bytes <= PREVIEW_BYTES_BUDGET) break;
        const size = bmp.width * bmp.height * 4;
        state.previewCache.delete(idx);
        bytes -= size;
        bmp.close();
    }
    state.previewBytes = bytes;
}

// ── 时间轴（分页元数据 + 可见区域） ─────────────────────────────────────
const pages = new TimelinePages(invoke);
state.current = null;
state.sourceViews = new Map();
state.selectionRange = null;
let querySerial = 0,
    hoverSerial = 0,
    stripSerial = 0,
    playGeneration = 0,
    editTail = Promise.resolve();

function selected(idx) {
    return state.selectedSet.has(idx) || (state.selectionRange && idx >= state.selectionRange[0] && idx <= state
        .selectionRange[1]);
}

function selectionRanges() {
    const all = [...state.selectedSet].map(i => [i, i]);
    if (state.selectionRange) all.push([...state.selectionRange]);
    all.sort((a, b) => a[0] - b[0]);
    const ranges = [];
    for (const [a, b] of all) {
        const last = ranges.at(-1);
        if (last && a <= last[1] + 1) last[1] = Math.max(last[1], b);
        else ranges.push([a, b]);
    }
    return ranges;
}

function selectionCount() {
    return selectionRanges().reduce((n, [a, b]) => n + b - a + 1, 0);
}

function currentPlaySrc() {
    return state.current?.srcIdx ?? null;
}

function layoutGeometry(view) {
    state.thumbW = Math.max(48, Math.round(THUMB_H * view.meta.width / view.meta.height));
    state.slotW = state.thumbW + THUMB_GAP;
    state.stripWidth = Math.min(10_000_000, view.frameCount * state.slotW);
    els.strip.style.width = `${state.stripWidth}px`;
    els.frameCountInd.textContent = t('gif.editor.frames', {
        n: view.frameCount
    });
}

function visibleRange() {
    const count = state.view?.frameCount || 0,
        visible = Math.ceil(els.stripScroll.clientWidth / state.slotW);
    const scroll = els.stripScroll.scrollLeft;
    const first = count * state.slotW <= 10_000_000 ? Math.floor(scroll / state.slotW) :
        Math.floor(scroll / Math.max(1, state.stripWidth - els.stripScroll.clientWidth) * Math.max(0, count - visible));
    return [Math.max(0, first - VIRTUAL_BUFFER), Math.min(count - 1, first + visible + VIRTUAL_BUFFER), first];
}
async function relayoutStrip() {
    if (!state.view) return;
    const serial = ++stripSerial,
        epoch = state.epoch;
    const [start, end, first] = visibleRange();
    try {
        for (let i = start; i <= end; i = (Math.floor(i / 256) + 1) * 256) {
            const values = await pages.sources(i);
            if (serial !== stripSerial || epoch !== state.epoch) return;
            for (const value of values) state.sourceViews.set(value.srcIdx, value);
        }
        // The DOM and metadata map contain only visible slots, independent of duration.
        for (const [i, element] of state.liveThumbs)
            if (i < start || i > end) {
                element.remove();
                state.liveThumbs.delete(i);
            }
        for (const i of state.sourceViews.keys())
            if (i < start || i > end) state.sourceViews.delete(i);
        const needed = [];
        for (let i = start; i <= end; i++) {
            const info = state.sourceViews.get(i);
            if (!info) continue;
            let button = state.liveThumbs.get(i);
            if (!button) {
                button = document.createElement('button');
                button.className = 'frame-thumb';
                button.dataset.idx = String(i);
                const img = document.createElement('img');
                img.style.height = `${THUMB_H}px`;
                img.style.width = '100%';
                img.style.objectFit = 'contain';
                button.appendChild(img);
                const tag = document.createElement('span');
                tag.className = 'frame-tag';
                button.appendChild(tag);
                button.addEventListener('click', e => handleThumbClick(i, e));
                button.addEventListener('contextmenu', e => {
                    e.preventDefault();
                    if (!selected(i)) {
                        state.selectionRange = null;
                        state.selectedSet = new Set([i]);
                    }
                    void editSelection(info.deleted ? 'restore' : 'delete');
                });
                els.strip.appendChild(button);
                state.liveThumbs.set(i, button);
            }
            const left = state.view.frameCount * state.slotW <= 10_000_000 ? i * state.slotW :
                els.stripScroll.scrollLeft + (i - first) * state.slotW;
            button.style.left = `${left}px`;
            button.style.width = `${state.thumbW}px`;
            button.classList.toggle('frame-deleted', info.deleted);
            button.classList.toggle('out-of-range', !info.inTrim && !info.deleted);
            button.classList.toggle('selected', Boolean(selected(i)));
            button.classList.toggle('playhead', currentPlaySrc() === i);
            button.querySelector('.frame-tag').textContent = formatFrameDur(info.durMs) + (info.held ? ' H' : '');
            const url = state.thumbCache.get(i);
            if (url) button.querySelector('img').src = url;
            else needed.push(i);
            button.title = t(info.deleted ? 'gif.editor.thumb.deleted' : !info.inTrim ?
                'gif.editor.thumb.outOfRange' : 'gif.editor.thumb.frame', {
                    frame: i
                });
        }
        if (needed.length) requestDecode(state.sessionId, needed, 160, state.thumbCache);
    } catch (error) {
        if (epoch === state.epoch) showHint(normalizeError(error).message);
    }
}

function refreshLiveThumbs() {
    for (const [i, el] of state.liveThumbs) {
        const url = state.thumbCache.get(i);
        if (url) el.querySelector('img').src = url;
    }
    if (state.seekPreviewSrc != null) {
        const url = state.thumbCache.get(state.seekPreviewSrc);
        if (url) els.seekPreviewImg.src = url;
    }
}
els.stripScroll.addEventListener('scroll', () => void relayoutStrip(), {
    passive: true
});

function syncPlayhead(src, select = false) {
    if (src == null) return;
    if (select) {
        state.selectionRange = null;
        state.selectedSet = new Set([src]);
    }
    if (!state.liveThumbs.has(src)) {
        const count = state.view.frameCount;
        els.stripScroll.scrollLeft = count * state.slotW <= 10_000_000 ? Math.max(0, src * state.slotW - els.stripScroll
                .clientWidth / 2) :
            src / Math.max(1, count - 1) * Math.max(0, state.stripWidth - els.stripScroll.clientWidth);
    }
    void relayoutStrip();
}

function drawPreview(image) {
    const canvas = els.canvas;
    if (canvas.width !== image.width || canvas.height !== image.height) {
        canvas.width = image.width;
        canvas.height = image.height;
    }
    canvas.getContext('2d').drawImage(image, 0, 0, canvas.width, canvas.height);
    els.canvasEmpty.classList.add('hidden');
}

function presentPreview() {
    const image = readyPreview(state.previewCache, state.current?.srcIdx, state.playing);
    if (image) drawPreview(image);
}

function renderPreview() {
    const f = state.current;
    if (!f) {
        els.canvas.getContext('2d').clearRect(0,0,els.canvas.width,els.canvas.height);
        els.canvasEmpty.classList.remove('hidden');
        els.pos.textContent = '0 / 0';
        els.dur.textContent = '';
        els.totalDur.textContent = t('gif.editor.totalDur', {dur: formatSeconds(0)});
        updatePlayProgress();
        return;
    }
    const image = state.previewCache.get(f.srcIdx);
    if (image) {
        state.previewCache.delete(f.srcIdx);
        state.previewCache.set(f.srcIdx, image);
        drawPreview(image);
    } else {
        presentPreview();
        requestDecode(state.sessionId, [f.srcIdx], state.previewWidth, state.previewCache, true);
    }
    els.pos.textContent = `${f.playIdx + 1} / ${state.view.effectiveCount}`;
    els.dur.textContent = formatFrameDur(f.durMs) + (f.held ? t('gif.editor.held') : '');
    els.totalDur.textContent = t('gif.editor.totalDur', {
        dur: formatSeconds(state.view.totalMs)
    });
    updatePlayProgress();
}
async function locate(request, select = false, inFrame = 0) {
    const serial = ++querySerial,
        epoch = state.epoch, revision = state.view?.revision;
    try {
        const frame = await pages.locate(request);
        if (!frame || serial !== querySerial || epoch !== state.epoch) return;
        state.current = frame;
        state.playIdx = frame.playIdx;
        state.seekInFrameMs = request.timeMs != null ? Math.min(frame.durMs, Math.max(0, request.timeMs - frame
            .startMs)) : inFrame;
        state.frameShownAt = performance.now() - state.seekInFrameMs;
        renderPreview();
        syncPlayhead(frame.srcIdx, select);
        // 播放只更新画面/播放头；参数面板不随每一帧反复赋值。
        if (select) renderControls();
        prefetchAhead();
        return frame;
    } catch (error) {
        if (epoch === state.epoch && serial === querySerial && revision === state.view?.revision) {
            // 编辑提交与旧版本分页请求可能交错；新 revision 会随编辑结果发布。
            if (state.editing && normalizeError(error).code === 'gif_revision') return;
            setPlaying(false);
            showHint(normalizeError(error).message);
        }
    }
}

function prefetchAhead() {
    const f = state.current;
    if (!f) return;
    const epoch = state.epoch;
    void pages.ahead(f.playIdx + 1, state.playing ? 16 : 2).then(frames => {
        if (epoch === state.epoch) requestDecode(state.sessionId, frames.map(next => next.srcIdx),
            state.previewWidth, state.previewCache);
    }).catch(() => {});
}

function positionMs() {
    if (!state.view) return 0;
    return state.playing ? (performance.now() - state.playClockAt) % Math.max(1, state.view.totalMs) :
        (state.current?.startMs || 0) + state.seekInFrameMs;
}

function setPlaying(on) {
    if (on && state.deleting) return;
    if (on === state.playing) return;
    playGeneration++;
    const at = positionMs();
    state.playing = on && Boolean(state.current);
    clearTimeout(state.playTimer);
    if (state.playing) {
        state.playClockAt = performance.now() - at;
        prefetchAhead();
        void playTick();
    } else {
        state.seekInFrameMs = Math.max(0, Math.min(state.current?.durMs || 0, at - (state.current?.startMs || 0)));
        audioPlayback.pause();
        void pumpDecode();
        reportPlayPosition();
    }
    const icons = els.btnPlay.querySelectorAll('.icon');
    icons[0]?.classList.toggle('hidden', state.playing);
    icons[1]?.classList.toggle('hidden', !state.playing);
}
async function playTick() {
    if (!state.playing || state.retiming) return;
    const epoch = state.epoch,
        generation = playGeneration,
        at = positionMs();
    if (!state.current || at < state.current.startMs || at >= state.current.startMs + state.current.durMs)
        await locate({
            timeMs: Math.floor(at)
        });
    if (!state.playing || state.retiming || epoch !== state.epoch || generation !== playGeneration) return;
    updatePlayProgress();
    state.playTimer = setTimeout(playTick, Math.min(16, Math.max(1, (state.current?.startMs || 0) + (state.current
        ?.durMs || 16) - positionMs())));
}

function updatePlayProgress() {
    const total = state.view?.totalMs || 0,
        at = positionMs(),
        percent = total > 0 ? at / total * 100 : 0;
    els.playProgress.style.width = `${percent}%`;
    els.seekThumb.style.left = `${percent}%`;
    els.seekBar.setAttribute('aria-disabled', total > 0 ? 'false' : 'true');
    els.seekBar.setAttribute('aria-valuemax', String(total));
    els.seekBar.setAttribute('aria-valuenow', String(Math.round(at)));
    els.seekBar.setAttribute('aria-valuetext', formatTimecode(at));
    els.seekTime.textContent = `${formatTimecode(at)} / ${formatTimecode(total)}`;
    const f = state.current;
    if (state.playing && !state.retiming && f) audioPlayback.sync(Math.max(0, at - f.startMs), state.previewSound,
        [{
            sourceMs: f.sourceMs,
            key: f.audioKey,
            speed: f.speed,
            soundMs: f.soundMs,
            silenceMs: f.silenceMs,
            outputMs: f.durMs
        }], state.previewMix || state.view.mix);
}

function handleThumbClick(i, event) {
    if (event.shiftKey && state.anchor != null) {
        state.selectedSet.clear();
        state.selectionRange = [Math.min(i, state.anchor), Math.max(i, state.anchor)];
    } else if (event.ctrlKey || event.metaKey) {
        state.selectionRange = null;
        if (state.selectedSet.has(i)) state.selectedSet.delete(i);
        else state.selectedSet.add(i);
        state.anchor = i;
    } else {
        state.selectionRange = null;
        state.selectedSet = new Set([i]);
        state.anchor = i;
        setPlaying(false);
        void locate({
            source: i
        });
    }
    void relayoutStrip();
    renderControls();
}
async function editSelection(operation, ms = 0) {
    const ranges = selectionRanges();
    if (!ranges.length) return;
    await edit({
        type: 'edit_ranges',
        operation,
        ranges,
        ms
    });
}

let durationKey = '', durationSerial = 0;
function refreshSelectionDuration() {
    const ranges = selectionRanges(), n = selectionCount();
    els.durInput.disabled = !n;
    if (!n) {
        durationKey = ''; durationSerial++;
        els.durClear.disabled = true;
        els.durInput.value = ''; els.durInput.placeholder = '—';
        return;
    }
    const key = `${state.epoch}:${state.view.revision}:${JSON.stringify(ranges)}`;
    if (key === durationKey) return;
    durationKey = key;
    const serial = ++durationSerial;
    els.durClear.disabled = true;
    void pages.selection(ranges).then(summary => {
        if (serial !== durationSerial || !summary || summary.revision !== state.view?.revision) return;
        els.durClear.disabled = !summary.hasHold;
        if (document.activeElement !== els.durInput) {
            els.durInput.value = summary.durationMs == null ? '' : String(summary.durationMs);
            els.durInput.placeholder = n > 1 ? t('gif.editor.durPh.multi', {n}) : '—';
        }
    }).catch(error => {
        if (serial === durationSerial) showHint(normalizeError(error).message);
    });
}

function renderControls() {
    const v = state.view;
    if (!v) return;
    els.undo.disabled = !v.canUndo;
    els.redo.disabled = !v.canRedo;
    const speed = String(v.speed), loop = v.loopCount == null ? '' : String(v.loopCount);
    if (els.speed.value !== speed) els.speed.value = speed;
    if (els.loop.value !== loop) els.loop.value = loop;
    syncEditorSelects();
    els.exportBtn.disabled = !v.effectiveCount;
    refreshSelectionDuration();
}

function renderSessionHeader() {
    els.title.textContent = t('gif.editor.title');
    const m = state.view?.meta;
    els.meta.textContent = m ? t('gif.editor.meta', {
        w: m.width,
        h: m.height,
        fps: m.fps,
        n: state.view.frameCount
    }) : '';
}

function renderAudioMix(view) {
    for (let i = 0; i < 2; i++) {
        const row = $(`audio-mix-${i}`),
            mix = (state.previewMix || view.mix)?.[i] || {
                gain: 1,
                muted: false
            };
        row.classList.toggle('hidden', !view.audio?.enabled?.[i]);
        row.querySelector('input[type="range"]').value = String(mix.gain * 100);
        row.querySelector('input[type="checkbox"]').checked = mix.muted;
        row.querySelector('output').textContent = `${Math.round(mix.gain*100)}%`;
    }
    $('audio-mix').classList.toggle('hidden', !view.audio?.enabled?.some(Boolean));
    $('preview-audio').classList.toggle('hidden', !view.audio?.enabled?.some(Boolean));
    $('preview-audio-enabled').checked = state.previewSound;
}

function showHint(message, isError = true) {
    els.hint.textContent = message;
    els.hint.classList.toggle('hint-error', isError);
    els.hint.classList.remove('hidden');
    clearTimeout(state.hintTimer);
    state.hintTimer = setTimeout(() => els.hint.classList.add('hidden'), 4000);
}

function renderStopBanner(reason) {
    const key = ['overload', 'budget_exhausted', 'capture_failed'].includes(reason) ? `gif.editor.stop.${reason}` :
    null;
    els.stopBanner.classList.toggle('hidden', !key);
    if (key) els.stopBannerText.textContent = t(key);
}

function edit(command) {
    if (state.deleting) return Promise.resolve();
    const sessionId = state.sessionId,
        epoch = state.epoch;
    editTail = editTail.catch(() => {}).then(async () => {
        if (!sessionId || epoch !== state.epoch) return;
        const liveMix = command.type === 'set_audio' || command.type === 'set_loop';
        const liveSpeed = command.type === 'set_speed';
        if (!liveMix && !liveSpeed) setPlaying(false);
        if (!liveMix) state.previewMix = null;
        const source = currentPlaySrc();
        let retiming = false;
        state.editing = true;
        try {
            const view = await invoke('gif_edit', {
                sessionId,
                command,
                incremental: true,
                revision: state.view.revision
            });
            if (epoch !== state.epoch || view.revision < state.view.revision) return;
            if (liveMix) {
                // 混音/循环不改变帧位置，保留播放时钟与有界元数据缓存。
                state.view = {...state.view, ...view};
                pages.reset(sessionId, view.revision, true);
                renderAudioMix(state.view);
                renderControls();
                updatePlayProgress();
                exportController.invalidate?.();
                return;
            }
            let anchor = null;
            if (liveSpeed) {
                retiming = state.retiming = true;
                playGeneration++; clearTimeout(state.playTimer); querySerial++;
                const at = positionMs();
                const frame = pages.cached({timeMs: Math.floor(at)}) || state.current;
                if (frame) anchor = {source: frame.srcIdx, ratio: Math.max(0, Math.min(1, (at - frame.startMs) / frame.durMs))};
            }
            state.view = {
                ...state.view,
                ...view
            };
            pages.reset(sessionId, view.revision);
            state.sourceViews.clear();
            hideSeekPreview();
            for (const el of state.liveThumbs.values()) el.remove();
            state.liveThumbs.clear();
            renderAudioMix(state.view);
            renderControls();
            if (view.effectiveCount) {
                const target = anchor?.source ?? source;
                const frame = await locate(target == null ? {play: 0} : {source: target});
                if (epoch !== state.epoch) return;
                if (anchor && frame) {
                    state.seekInFrameMs = anchor.ratio * frame.durMs;
                    state.frameShownAt = performance.now() - state.seekInFrameMs;
                    if (state.playing) state.playClockAt = performance.now() - frame.startMs - state.seekInFrameMs;
                    renderPreview();
                }
            }
            else {
                state.current = null;
                state.playIdx = 0;
                state.seekInFrameMs = 0;
                renderPreview();
            }
            void relayoutStrip();
            exportController.invalidate?.();
        } catch (error) {
            if (epoch === state.epoch) showHint(normalizeError(error).message);
        } finally {
            if (epoch === state.epoch) state.editing = false;
            if (retiming && epoch === state.epoch) {
                state.retiming = false;
                if (state.playing) void playTick();
            }
        }
    });
    return editTail;
}
async function loadSession(sessionId) {
    setPlaying(false);
    durationSerial++; durationKey = '';
    els.durClear.disabled = true;
    els.durInput.disabled = true;
    state.retiming = false;
    state.editing = false;
    state.previewMix = null;
    audioPlayback.clear();
    const epoch = ++state.epoch;
    querySerial++;
    hoverSerial++;
    stripSerial++;
    state.sessionId = sessionId;
    state.view = null;
    state.current = null;
    els.canvas.getContext('2d').clearRect(0,0,els.canvas.width,els.canvas.height);
    els.canvasEmpty.classList.remove('hidden');
    state.seekInFrameMs = 0;
    state.seekDragging = false;
    state.seekWasPlaying = false;
    state.seekRequestedMs = null;
    state.selectionRange = null;
    state.selectedSet.clear();
    state.sourceViews.clear();
    clearTimeout(state.posReportTimer);
    state.thumbCache.clear();
    for (const b of state.previewCache.values()) b.close();
    state.previewCache.clear();
    state.previewBytes = 0;
    state.pendingBatches.length = 0;
    state.decodeKeys.clear();
    for (const e of state.liveThumbs.values()) e.remove();
    state.liveThumbs.clear();
    hideSeekPreview();
    exportController.setSession(sessionId);
    pages.reset(sessionId, 0);
    if (!sessionId) {
        showRecentView();
        return;
    }
    const view = await invoke('gif_get_session', {
        sessionId
    });
    if (epoch !== state.epoch) return;
    state.view = view;
    exportController.setSource?.({
        fps: view.meta.fps
    });
    pages.reset(sessionId, view.revision);
    state.previewWidth = Math.min(view.meta.width, Math.max(80, Math.ceil(($('preview-stage').clientWidth || 800) *
        devicePixelRatio)), 1280);
    void audioPlayback.load(sessionId, view.audio?.enabled);
    renderAudioMix(view);
    renderSessionHeader();
    renderStopBanner(view.meta.stopReason);
    layoutGeometry(view);
    showEditorView();
    renderControls();
    await locate({
        timeMs: Math.min(Number(view.draftPlayPosMs) || 0, Math.max(0, view.totalMs - 1))
    });
    void relayoutStrip();
    if (view.draftStatus === 'discarded') showHint(t('gif.editor.draftDiscarded'), false);
    if (view.audioRecovered) showHint(t('gif.audio.recovered'), false);
}

function hideSeekPreview() {
    hoverSerial++;
    state.seekPreviewSrc = null;
    els.seekPreview.classList.add('hidden');
    els.seekPreviewImg.removeAttribute('src');
}

async function endSeekDrag(resume) {
    if (!state.seekDragging) return;
    state.seekDragging = false;
    const epoch = state.epoch;
    // Pointer moves may fill the bounded metadata slots. Always commit the last
    // requested position after a slot becomes available, including while paused.
    while (pages.inflight.size >= 4) {
        await Promise.race([...pages.inflight].map(p => p.catch(() => null)));
        if (epoch !== state.epoch || state.seekDragging) return;
    }
    const frame = await locate({timeMs: Math.floor(state.seekRequestedMs ?? positionMs())});
    if (epoch !== state.epoch || state.seekDragging) return;
    if (frame && resume && state.seekWasPlaying) setPlaying(true);
    reportPlayPosition();
}
async function seekAt(clientX, hover = false) {
    if (!state.view?.totalMs) return;
    const serial = ++hoverSerial,
        epoch = state.epoch,
        rect = els.seekBar.getBoundingClientRect();
    const time = ratioToTime(pointerRatio(clientX, rect), state.view.totalMs);
    if (!hover) {
        state.seekRequestedMs = time;
        await locate({
            timeMs: Math.floor(time)
        });
        reportPlayPosition();
        return;
    }
    const f = await pages.locate({
        timeMs: Math.floor(time)
    });
    if (!f || serial !== hoverSerial || epoch !== state.epoch) return;
    state.seekPreviewSrc = f.srcIdx;
    els.seekPreviewLabel.textContent = t('gif.editor.seek.preview', {
        frame: f.srcIdx,
        time: formatTimecode(time)
    });
    els.seekPreviewImg.removeAttribute('src');
    const url = state.thumbCache.get(f.srcIdx);
    if (url) els.seekPreviewImg.src = url;
    else requestDecode(state.sessionId, [f.srcIdx], 160, state.thumbCache);
    els.seekPreview.style.left =
        `${clampPreviewLeft(clientX-rect.left,els.seekPreview.offsetWidth||120,rect.width)}px`;
    els.seekPreview.classList.remove('hidden');
}
els.seekBar.addEventListener('pointerdown', e => {
    if (e.button !== 0) return;
    e.preventDefault();
    state.seekWasPlaying = state.playing;
    setPlaying(false);
    state.seekDragging = true;
    els.seekBar.setPointerCapture(e.pointerId);
    els.seekBar.focus({
        preventScroll: true
    });
    hideSeekPreview();
    void seekAt(e.clientX);
});
els.seekBar.addEventListener('pointermove', e => {
    if (state.seekDragging) void seekAt(e.clientX);
    else if (e.pointerType === 'mouse') void seekAt(e.clientX, true).catch(() => {});
});
els.seekBar.addEventListener('pointerup', () => endSeekDrag(true));
els.seekBar.addEventListener('pointercancel', () => endSeekDrag(false));
els.seekBar.addEventListener('pointerleave', hideSeekPreview);
els.seekBar.addEventListener('keydown', e => {
    const next = keyboardStep(state.playIdx, state.view?.effectiveCount || 0, e.code);
    if (next != null) {
        e.preventDefault();
        setPlaying(false);
        void locate({
            play: next
        });
    }
});

function stepFrame(delta) {
    if (!state.view?.effectiveCount) return;
    setPlaying(false);
    void locate({
        play: (state.playIdx + delta + state.view.effectiveCount) % state.view.effectiveCount
    }, true);
}
$('btn-play').addEventListener('click', () => setPlaying(!state.playing));
$('btn-prev').addEventListener('click', () => stepFrame(-1));
$('btn-next').addEventListener('click', () => stepFrame(1));
$('btn-trim-start').addEventListener('click', () => {
    const frame = currentPlaySrc();
    if (frame != null) void edit(commands.trimStart(frame));
});
$('btn-trim-end').addEventListener('click', () => {
    const frame = currentPlaySrc();
    if (frame != null) void edit(commands.trimEnd(frame));
});
els.undo.addEventListener('click', () => edit(commands.undo()));
els.redo.addEventListener('click', () => edit(commands.redo()));
$('btn-reset').addEventListener('click', () => edit({
    type: 'reset'
}));
els.speed.addEventListener('change', () => edit(commands.setSpeed(Number(els.speed.value))));
els.loop.addEventListener('change', () => edit(commands.setLoop(els.loop.value === '' ? null : Number(els.loop
.value))));
els.btnDelete.addEventListener('click', () => editSelection('delete'));
els.durClear.addEventListener('click', () => {
    els.durClear.disabled = true;
    return editSelection('hold', 0);
});
els.durInput.addEventListener('change', () => {
    const n = Number(els.durInput.value);
    if (els.durInput.value.trim() && Number.isFinite(n)) void editSelection('hold', Math.min(6000, Math.max(10, n)));
});
els.durInput.addEventListener('blur', () => {
    durationKey = '';
    if (state.view) refreshSelectionDuration();
});
let audioMixVersion = 0;
for (let i = 0; i < 2; i++) {
    const row = $(`audio-mix-${i}`);
    const readMix = () => ({
        gain: Number(row.querySelector('input[type="range"]').value) / 100,
        muted: row.querySelector('input[type="checkbox"]').checked
    });
    const previewMix = () => {
        state.previewMix = Array.from({length: 2}, (_, source) => ({gain: 1, muted: false, ...(state.previewMix || state.view.mix)?.[source]}));
        state.previewMix[i] = readMix();
        updatePlayProgress();
    };
    row.addEventListener('input', () => {
        audioMixVersion++;
        row.querySelector('output').textContent = `${row.querySelector('input[type="range"]').value}%`;
        if (state.view) previewMix();
    });
    row.addEventListener('change', () => {
        const version = ++audioMixVersion, epoch = state.epoch;
        previewMix();
        return edit({type: 'set_audio', source: i, ...readMix()}).finally(() => {
            if (epoch !== state.epoch || version !== audioMixVersion) return;
            state.previewMix = null;
            renderAudioMix(state.view); updatePlayProgress();
        });
    });
}
$('preview-audio-enabled').addEventListener('change', event => {
    state.previewSound = event.target.checked;
    // 预览静音只暂停源轨；继续播放的画面不必反复释放/重开音频上下文。
    updatePlayProgress();
});
els.stopBannerClose.addEventListener('click', () => els.stopBanner.classList.add('hidden'));
document.addEventListener('keydown', e => {
    const target = document.activeElement;
    if (els.dialog.open || !state.view || els.editorRoot.classList.contains('hidden') ||
        e.isComposing || e.ctrlKey || e.altKey || e.metaKey ||
        target?.isContentEditable || target?.tagName === 'TEXTAREA' || target?.tagName === 'SELECT' ||
        (target?.tagName === 'INPUT' && !['checkbox', 'radio', 'range', 'button'].includes(target.type)) ||
        target?.closest('.editor-select-menu')) return;
    if (e.code === 'Space') {
        e.preventDefault(); e.stopPropagation();
        if (!e.repeat) setPlaying(!state.playing);
        return;
    }
    if (target?.tagName === 'INPUT' || target?.closest('.editor-select-trigger')) return;
    if (document.activeElement === els.seekBar && e.code !== 'Space' && !['Delete', 'Backspace'].includes(e
            .code)) return;
    if (e.code === 'ArrowLeft') {
        e.preventDefault();
        stepFrame(-1);
    } else if (e.code === 'ArrowRight') {
        e.preventDefault();
        stepFrame(1);
    } else if (['Delete', 'Backspace'].includes(e.code) && selectionCount()) {
        e.preventDefault();
        void editSelection('delete');
    }
}, true);
const exportController = createExportController();
$('btn-discard-recording').addEventListener('click', async () => {
    const sessionId = state.sessionId;
    if (!sessionId || state.deleting) return;
    state.deleting = true;
    exportController.setLocked(true);
    const button = $('btn-discard-recording');
    button.disabled = true;
    els.btnRecent.disabled = true;
    try {
        if (!await confirmDialog(t('gif.editor.discardConfirm'))) return;
        if (state.sessionId !== sessionId) return;
        setPlaying(false);
        audioPlayback.clear();
        await editTail;
        ++state.epoch;
        state.pendingBatches.length = 0;
        showHint(t('gif.editor.discarding'), false);
        await discardRecording({sessionId, invoke,
            isCurrent: () => state.sessionId === sessionId,
            pendingStart: exportController.pendingStart,
        });
        if (state.sessionId === sessionId) await loadSession(null);
    } catch (error) {
        if (state.sessionId === sessionId) {
            await loadSession(sessionId).catch(() => {});
            showHint(t('gif.editor.discardFailed', {msg: normalizeError(error).message}));
        }
    } finally {
        state.deleting = false;
        button.disabled = false;
        els.btnRecent.disabled = false;
        exportController.setLocked(false);
    }
});
$('btn-export').addEventListener('click', () => setPlaying(false));
window.addEventListener('beforeunload', () => exportController.dispose());
// ── 视图切换 / 最近录制（0.28.23 §5.7.4）────────────────────────────────

function refreshPreviewSize() {
    if (!state.view) return;
    const width = Math.min(state.view.meta.width, Math.max(80, Math.ceil(($('preview-stage').clientWidth || 800) * devicePixelRatio)), 1280);
    if (width === state.previewWidth) return;
    state.previewWidth = width;
    for (const image of state.previewCache.values()) image.close();
    state.previewCache.clear();state.previewBytes = 0;
    renderPreview();
}
if (typeof ResizeObserver === 'function') new ResizeObserver(refreshPreviewSize).observe($('preview-stage'));

function showEditorView() {
    els.editorRoot?.classList.remove('hidden');
    els.recentPane?.classList.add('hidden');
}

function showRecentView() {
    setPlaying(false);
    hideSeekPreview();
    els.editorRoot?.classList.add('hidden');
    els.recentPane?.classList.remove('hidden');
    void recentList.refresh();
}

const recentList = createRecentList({
    invoke,
    t,
    errorText: error => normalizeError(error).message,
    confirm: message => confirmDialog(message),
    onOpened: () => {
        // 会话切换由后端 open_editor 发事件驱动 loadSession；此处只切视图
        showEditorView();
    },
    currentSessionId: () => state.sessionId,
});
recentList.bind();
els.btnRecent?.addEventListener('click', () => {
    reportPlayPosition(true);
    showRecentView();
});
els.btnBackEdit?.addEventListener('click', () => {
    if (state.sessionId) showEditorView();
});

/** 上报播放头位置给草稿（节流 500ms；`immediate` 用于切换/关闭前）。 */
function reportPlayPosition(immediate = false) {
    if (state.deleting) return;
    if (!state.sessionId || !state.view) return;
    const sessionId = state.sessionId;
    const send = () => {
        if (state.deleting || sessionId !== state.sessionId || !state.view) return;
        const offset = playbackOffset(state.current?.durMs || 0,
            state.playing, state.frameShownAt, state.seekInFrameMs, performance.now());
        const pos = Math.round((state.current?.startMs || 0) + offset);
        invoke('gif_set_play_position', {
            sessionId,
            posMs: pos
        }).catch(() => {});
    };
    clearTimeout(state.posReportTimer);
    if (immediate) send();
    else state.posReportTimer = setTimeout(send, 500);
}

window.addEventListener('pagehide', () => reportPlayPosition(true));

const sessionListener = listen(EVENTS.GIF_EDITOR_SESSION, event => {
    if (event.payload?.recordingSaved) showHint(t('gif.editor.recordingSaved'), false);
    if (event.payload?.discardFailed) showHint(t('gif.rec.discardCleanupFailed'));
    if (event.payload?.discardedSessionId && event.payload.discardedSessionId !== state.sessionId) return;
    const sid = event.payload?.sessionId;
    if (!sid) {
        // 空会话（主窗口入口无活动会话）：进入最近录制首页
        void loadSession(null);
        return;
    }
    if (sid !== state.sessionId) {
        loadSession(sid).catch(err => console.error('[gif-editor] 会话加载失败', err));
    } else {
        showEditorView();
    }
});

// ── i18n / 窗口标题（0.28.23）────────────────────────────────────────────
// 录制编辑器原为硬编码中文；接入 i18n 后静态文案走 data-i18n，动态文案在
// 语言切换时按当前视图重算。原生窗口标题经 set-title 同步（ACL 已含）。

function applyWindowTitle() {
    getCurrentWindow()?.setTitle(t('gif.editor.windowTitle'))?.catch(() => {});
    document.title = t('gif.editor.windowTitle');
}

/** 语言切换后刷新全部 JS 动态文案（静态 data-i18n 由 applyI18n 处理）。 */
function refreshLocalizedTexts() {
    syncEditorSelects();
    applyWindowTitle();
    if (state.view) {
        renderSessionHeader();
        renderStopBanner(state.view.meta?.stopReason);
        layoutGeometry(state.view);
        for (const [, el] of state.liveThumbs) el.remove();
        state.liveThumbs.clear();
        relayoutStrip();
        renderPreview();
        renderControls();
    }
}

onLangChange(refreshLocalizedTexts);
listen(EVENTS.CONFIG_CHANGED, event => {
    if (!event.payload?.key || ['appearance', 'general_config', 'theme'].includes(event.payload.key)) void applyThemeFromConfig();
    if (!event.payload?.key || event.payload.key === 'language') {
        void applyI18nFromConfig();
    }
});
void applyI18nFromConfig();

sessionListener.then(async () => {
        const epoch = state.epoch;
        const sid = await invoke('gif_editor_current');
        if (epoch !== state.epoch) return;
        return loadSession(sid);
    })
    .catch(err => console.error('[gif-editor] 初始会话拉取失败', normalizeError(err)));
