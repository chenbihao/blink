//! 录制编辑器（0.28.7 建，gif-editor 窗口；0.28.23 改名并加进度定位）。
//!
//! 结构：
//! - 状态真源在后端（domain Timeline）；本页持视图快照 + 素材帧图缓存。
//! - **时间轴**：ScreenToGif 式全量连续缩略图——虚拟化渲染（只建可视区
//!   ±缓冲帧的 DOM，总宽按帧数撑起），滚动到视口时批量懒解码
//!   （`gif_get_frames` ≤24 帧/批、串行队列防并发），无分页。
//! - **预览播放**：setTimeout 链按帧 `durMs` 调度；空格启动先等当前帧
//!   剩余时长（不丢帧节奏）；播放头前方预取窗口（+24 帧）后台解码，
//!   缓存 miss 时保持上一帧画面不闪；播放头实时同步时间轴高亮、滚动跟随
//!   与帧时长输入框回显。
//! - **进度定位**（0.28.23）：`#play-progress` 为可聚焦滑块——单击定位、按住
//!   拖动连续浏览（拖动起点暂停、松手按拖动前状态恢复）、方向键/Home/End、
//!   悬浮显示目标帧与时间。定位按编辑后有效时长前缀和（`durPrefix`）二分
//!   映射（seek-bar.js），只移动**播放头**（`.playhead`），不改编辑多选集。
//!   拖动中只用已有缓存/缩略图降级绘制，防抖后才对最新目标请求全尺寸解码；
//!   解码按 srcIdx 入缓存（内容恒正确，仅可能多余），旧会话回流经 epoch 丢弃。
//! - **预览清晰度**：解码宽度 = 预览显示区物理像素（clamp 到源宽），不在
//!   显示时拉伸；预览缓存按字节预算淘汰。
//! - 选中模型：`selectedSet`——单击单选、Ctrl 切换、Shift 范围；Delete/右键
//!   删除（单帧 `delete_frame`、多帧 `delete_frames` 批量单 undo）。
//!
//! 键盘：Space 播放/暂停；←/→ 帧步进；Delete/Backspace 删除选中；进度条聚焦
//! 时 ←/→/Home/End/PageUp/PageDown 走定位语义（不循环、不改选中）。

import {createExportController} from './export-controller.js';
import {ensureSpriteLoaded} from '../shared/icon.js';
import {confirmDialog, getCurrentWindow, invoke, listen, normalizeError} from '../shared/tauri.js';
import {EVENTS} from '../shared/event-names.js';
import {applyI18nFromConfig, onLangChange, t} from '../i18n/index.js';
import {commands, formatFrameDur, formatSeconds, frameState, nextPlayStep, playToSrc, srcToPlayIndex} from './timeline-view.js';
import {clampPreviewLeft, formatTimecode, keyboardStep, locateTime, playbackOffset, pointerRatio, ratioToTime, timeAt} from './seek-bar.js';
import {createRecentList} from './recent-list.js';
import {createAudioPlayback} from './audio-playback.js';

ensureSpriteLoaded();
const audioPlayback = createAudioPlayback({onError: error => showHint(t('gif.audio.previewFailed', {msg: normalizeError(error).message}))});
window.addEventListener('pagehide', () => audioPlayback.dispose());

/** 缩略图固定高度（CSS px）。 */
const THUMB_H = 64;
const THUMB_GAP = 6;
/** 可视区外仍保留的缓冲帧数（虚拟化窗口）。 */
const VIRTUAL_BUFFER = 10;
/** 播放头前方预取帧数。 */
const PLAY_PREFETCH = 24;
/** 预览缓存字节预算（ImageBitmap 按源像素估算）。 */
const PREVIEW_BYTES_BUDGET = 96 << 20;
/** 拖动定位中对最新目标请求全尺寸解码的防抖（ms）。 */
const SEEK_DECODE_DEBOUNCE_MS = 80;

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
    epoch: 0,
    view: null,
    playIdx: 0,
    selectedSet: new Set(),
    anchor: null,
    playing: false,
    playTimer: 0,
    /** 播放时钟的绝对到期时刻（漂移补偿基准；performance.now 系）。 */
    playDue: 0,
    /** 当前帧开始展示的时刻（空格启动时据此保留剩余时长）。 */
    frameShownAt: 0,
    /** 时间轴几何（会话加载时按宽高比计算）。 */
    thumbW: 96,
    slotW: 102,
    /** 预览解码宽度（显示区物理像素，clamp 到源宽）。 */
    previewWidth: 480,
    // 缩略图直接缓存后端返回的 PNG data URL（字符串）——不再转 ImageBitmap
    // 后又 canvas 重编码回 PNG（每个缩略图一次主线程同步编码，几百帧可观）。
    thumbCache: new Map(),   // srcIdx -> data URL（160 档缩略）
    previewCache: new Map(), // srcIdx -> ImageBitmap（previewWidth 档）
    previewBytes: 0,
    /** 素材帧号 → 有效时长（帧标签显示用；编辑后重建）。 */
    durBySrc: new Map(),
    /** 播放时间轴前缀和：prefix[i] = 播放序列前 i 帧累计时长（进度条用）。 */
    durPrefix: [0],
    /** 进度条 rAF 句柄（播放时驱动帧内连续推进）。 */
    progressRaf: 0,
    /** 已建 DOM 的帧槽（虚拟化窗口内）。 */
    liveThumbs: new Map(),   // srcIdx -> HTMLElement
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
    /** 拖动/定位中的全尺寸解码防抖句柄。 */
    seekDecodeTimer: 0,
    /** 进度条定位后的帧内偏移（暂停态显示/恢复播放用；步进/点选/播放推进归零）。 */
    seekInFrameMs: 0,
    /** 播放位置上报节流句柄（草稿播放头；提交定位/暂停/步进时上报）。 */
    posReportTimer: 0,
    seekPreviewSrc: null,
};

// ── 帧解码（批量懒加载 + 缓存淘汰） ─────────────────────────────────────

function requestDecode(sessionId, indices, maxW, cache, priority = false) {
    const epoch = state.epoch;
    const unique = [];
    for (const idx of indices) {
        if (cache.has(idx)) continue;
        const key = decodeKey(epoch, cache, maxW, idx);
        if (state.decodeKeys.has(key)) {
            // 已排队的目标提升优先级；在途目标复用原请求。
            if (priority) {
                const at = state.pendingBatches.findIndex(b => b.epoch === epoch && b.cache === cache
                    && b.maxW === maxW && b.indices.includes(idx));
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
        batches.push({sessionId, indices: indices.slice(i, i + 24), maxW, cache, epoch: state.epoch});
    }
    // priority（播放预取/当前预览）插队队首——后台顺序预载让位于交互
    if (priority) state.pendingBatches.unshift(...batches);
    else state.pendingBatches.push(...batches);
    pumpDecode();
}

function decodeKey(epoch, cache, maxW, idx) {
    return `${epoch}:${cache === state.thumbCache ? 'thumb' : 'preview'}:${maxW}:${idx}`;
}

async function pumpDecode() {
    if (state.decoding) return;
    state.decoding = true;
    while (state.pendingBatches.length) {
        const {sessionId, indices, maxW, cache, epoch} = state.pendingBatches.shift();
        try {
            const missing = indices.filter(i => !cache.has(i) && i < (state.view?.frameCount ?? 0));
            if (!missing.length || sessionId !== state.sessionId || epoch !== state.epoch) continue;
            for (let i = 0; i < missing.length; i += 24) {
                const batch = missing.slice(i, i + 24);
                try {
                    const frames = await invoke('gif_get_frames', {sessionId, indices: batch, maxW});
                    if (epoch !== state.epoch) break;
                    for (const f of frames) {
                        if (cache === state.thumbCache) {
                            cache.set(f.index, `data:image/png;base64,${f.pngBase64}`);
                        } else {
                            const blob = await (await fetch(`data:image/png;base64,${f.pngBase64}`)).blob();
                            const bmp = await createImageBitmap(blob);
                            if (epoch !== state.epoch) {
                                bmp.close();
                                break;
                            }
                            const old = cache.get(f.index);
                            if (old) {
                                state.previewBytes -= old.width * old.height * 4;
                                old.close();
                            }
                            cache.set(f.index, bmp);
                            state.previewBytes += bmp.width * bmp.height * 4;
                        }
                        evictPreviewCache();
                    }
                } catch (err) {
                    console.error('[gif-editor] 帧解码失败', err);
                }
                if (epoch !== state.epoch) break;
            }
            evictPreviewCache();
            refreshLiveThumbs();
            // 解码完成按当前会话/播放头回填，避免每次 cache miss 建永久轮询器。
            const current = state.view?.frames?.[state.playIdx]?.srcIdx;
            if (epoch === state.epoch && cache === state.previewCache && cache.has(current)) {
                drawPreview(cache.get(current));
            }
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

// ── 时间轴（虚拟化连续渲染） ────────────────────────────────────────────

function layoutGeometry(view) {
    const aspect = (view.meta?.width || 16) / (view.meta?.height || 9);
    state.thumbW = Math.max(48, Math.round(THUMB_H * aspect));
    state.slotW = state.thumbW + THUMB_GAP;
    els.strip.style.width = `${view.frameCount * state.slotW}px`;
    els.frameCountInd.textContent = t('gif.editor.frames', {n: view.frameCount});
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
            const url = state.thumbCache.get(idx);
            if (url) img.src = url;
        }
    }
    const url = state.thumbCache.get(state.seekPreviewSrc);
    if (state.seekPreviewSrc != null && url && els.seekPreviewImg) els.seekPreviewImg.src = url;
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
    // 已删（可右键恢复）与修剪外（只是不导出）是两种状态，视觉必须可区分
    if (fs.deleted) btn.classList.add('frame-deleted');
    else if (!fs.visible) btn.classList.add('out-of-range');
    if (state.selectedSet.has(idx)) btn.classList.add('selected');
    if (currentPlaySrc() === idx) btn.classList.add('playhead');
    const img = document.createElement('img');
    img.style.height = `${THUMB_H}px`;
    img.style.width = '100%';
    img.style.objectFit = 'contain';
    const url = state.thumbCache.get(idx);
    if (url) img.src = url;
    btn.appendChild(img);
    const tag = document.createElement('span');
    tag.className = 'frame-tag';
    // 有效时长（hold/速度编辑后的真实播放值）；已删/修剪外帧回退素材原始值
    tag.textContent = formatFrameDur(state.durBySrc.get(idx) ?? rawFrameDur(view, idx));
    btn.appendChild(tag);
    if (fs.held) {
        const hold = document.createElement('span');
        hold.className = 'hold-tag';
        hold.textContent = 'H';
        btn.appendChild(hold);
    }
    btn.title = fs.deleted ? t('gif.editor.thumb.deleted')
        : !fs.inTrim ? t('gif.editor.thumb.outOfRange')
        : fs.held ? t('gif.editor.thumb.held', {frame: idx, dur: formatFrameDur(view.holds[String(idx)])})
        : t('gif.editor.thumb.frame', {frame: idx});
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

/** 更新全部已建槽的选中态（选择集变化后）。 */
function refreshSelectionClasses() {
    for (const [idx, el] of state.liveThumbs) {
        el.classList.toggle('selected', state.selectedSet.has(idx));
    }
}

/** 更新全部已建槽的播放头标记（与选中态独立）。 */
function refreshPlayheadClass() {
    const cur = currentPlaySrc();
    for (const [idx, el] of state.liveThumbs) {
        el.classList.toggle('playhead', idx === cur);
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
        updatePlayProgress();
        return;
    }
    const cur = Math.min(state.playIdx, view.frames.length - 1);
    const srcIdx = view.frames[cur].srcIdx;
    const bmp = state.previewCache.get(srcIdx);
    if (bmp) {
        // LRU 触碰：命中重插到队尾。Map 插入序淘汰近似 FIFO——循环播放中
        // 命中不重排会把"刚播放过"的帧当最旧逐出，触发 逐出→重解码→
        // 缩略图降级 的抖动链（多循环几轮后预览频繁变模糊/变小的根因）。
        state.previewCache.delete(srcIdx);
        state.previewCache.set(srcIdx, bmp);
        drawPreview(bmp);
    } else if (state.seekDragging) {
        // 拖动定位中：只用缩略图降级绘制，不逐帧发解码请求/建等待定时器；
        // 全尺寸解码由 scheduleSeekDecode 防抖后对最新目标单独请求。
        drawThumbFallback(srcIdx);
    } else {
        // miss：先上缩略图降级帧（帧内容正确优先于清晰度，图片与播放头
        // /高亮不再错位），再请求全尺寸解码；完成后播放头未变才上屏。
        // 降级绘制不改写 canvas 位图尺寸（按预览档位初始化后只拉伸绘制）
        // ——canvas 显示尺寸跟随位图（max-width:100%），被缩略图尺寸定型
        // 就是"大图变小"的直接表现。
        drawThumbFallback(srcIdx);
        requestDecode(state.sessionId, [srcIdx], state.previewWidth, state.previewCache, true);
    }
    els.pos.textContent = `${cur + 1} / ${view.frames.length}`;
    els.dur.textContent = formatFrameDur(view.frames[cur].durMs) + (view.frames[cur].held ? t('gif.editor.held') : '');
    els.totalDur.textContent = t('gif.editor.totalDur', {dur: formatSeconds(view.totalMs)});
    refreshPlayheadClass();
    // 非播放态（步进/点选/编辑后）进度停在该帧起点；播放态由 rAF 循环推进
    updatePlayProgress();
}

/** 缩略图降级：有缓存则画到预览画布（不改位图尺寸）。 */
function drawThumbFallback(srcIdx) {
    const thumbUrl = state.thumbCache.get(srcIdx);
    if (!thumbUrl) return;
    const fallback = new Image();
    const epoch = state.epoch;
    fallback.onload = () => {
        if (epoch === state.epoch && currentPlaySrc() === srcIdx && !state.previewCache.has(srcIdx)) {
            drawFallback(fallback);
        }
    };
    fallback.src = thumbUrl;
}

function drawPreview(bmp) {
    const c = els.canvas;
    if (c.width !== bmp.width) c.width = bmp.width;
    if (c.height !== bmp.height) c.height = bmp.height;
    c.getContext('2d').drawImage(bmp, 0, 0);
    els.canvasEmpty.classList.add('hidden');
}

/** 缩略图降级绘制：不缩小 canvas 位图（按预览档位/首帧比例初始化），
 *  缩略图拉伸铺满——模糊但帧正确、尺寸稳定，全尺寸解码后无缝替换。 */
function drawFallback(img) {
    const c = els.canvas;
    if (c.width <= 0 || c.height <= 0) {
        const w = state.previewWidth || img.naturalWidth;
        const aspect = img.naturalHeight > 0 ? img.naturalHeight / img.naturalWidth : 1;
        c.width = w;
        c.height = Math.max(1, Math.round(w * aspect));
    }
    c.getContext('2d').drawImage(img, 0, 0, c.width, c.height);
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
    const total = state.durPrefix.at(-1) || 1;
    const elapsed = (performance.now() - state.playClockAt) % total;
    const loc = locateTime(state.durPrefix, elapsed);
    const step = {cursor: loc.playIdx, durMs: view.frames[loc.playIdx].durMs - loc.inFrameMs};
    state.playIdx = step.cursor;
    state.frameShownAt = performance.now() - loc.inFrameMs;
    state.seekInFrameMs = 0;
    renderPreview();
    prefetchAhead();
    syncPlayhead(playToSrc(view, step.cursor));
    syncDurInput();
    // 漂移补偿：按绝对时刻累计（playDue），setTimeout 只补差值——渲染耗时
    // 与 setTimeout 惰性不再逐帧累积，长会话播放不越播越慢
    const now = performance.now();
    state.playDue = now + Math.max(5, step.durMs);
    scheduleNext(state.playDue - now);
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
    const duration = state.view?.frames[state.playIdx]?.durMs || 0;
    const elapsed = playbackOffset(duration, state.playing, state.frameShownAt,
        state.seekInFrameMs, performance.now());
    state.seekInFrameMs = elapsed;
    state.playing = on;
    if (!on) audioPlayback.pause();
    else state.playClockAt = performance.now() - timeAt(state.durPrefix, state.playIdx, elapsed);
    const icons = els.btnPlay.querySelectorAll('.icon');
    if (icons.length === 2) {
        icons[0].classList.toggle('hidden', on);
        icons[1].classList.toggle('hidden', !on);
    }
    clearTimeout(state.playTimer);
    cancelAnimationFrame(state.progressRaf);
    state.progressRaf = 0;
    if (on) {
        // 保留当前帧剩余展示时长（不立即跳帧——空格启动也丝滑）；进度条
        // 定位后从指针所在帧内偏移续播。
        const view = state.view;
        if (view && view.frames.length) {
            const cur = Math.min(state.playIdx, view.frames.length - 1);
            state.seekInFrameMs = 0;
            state.frameShownAt = performance.now() - elapsed;
            const remaining = Math.max(5, view.frames[cur].durMs - elapsed);
            state.playDue = performance.now() + remaining;
            scheduleNext(remaining);
        }
        progressLoop();
    } else {
        updatePlayProgress();
        reportPlayPosition();
    }
}

/** 播放头轻量同步：切高亮类 + 滚动跟随（连续时间轴，无翻页）。
 *  `select=true`（播放/步进）同时把选中集置为播放头所在帧；进度条定位
 *  传 false——只移动播放头标记，不改编辑多选集合。 */
function syncPlayhead(srcIdx, select = true) {
    if (srcIdx == null) return;
    if (select) state.selectedSet = new Set([srcIdx]);
    const thumb = state.liveThumbs.get(srcIdx);
    if (thumb) {
        refreshSelectionClasses();
        refreshPlayheadClass();
        thumb.scrollIntoView({block: 'nearest', inline: 'nearest', behavior: 'auto'});
    } else {
        // 不在虚拟化窗口：滚到目标位置（scroll 事件触发 relayout 建槽）
        els.stripScroll.scrollLeft = Math.max(0, srcIdx * state.slotW - els.stripScroll.clientWidth / 2);
        requestAnimationFrame(() => {
            refreshSelectionClasses();
            refreshPlayheadClass();
            state.liveThumbs.get(srcIdx)
                ?.scrollIntoView({block: 'nearest', inline: 'nearest'});
        });
    }
}

// ── 选中模型 ─────────────────────────────────────────────────────────────

/** 编辑后播放头锚定：尽量停在同一个素材帧上；锚点帧被删则跳到其后
 *  第一个存活帧（srcIdx 升序），全删则落到末帧——此前 playIdx 数字不变
 *  但序列左移，删任何一帧预览都会跳帧、删末帧直接回到第 0 帧。 */
function anchorPlayIndex(view, srcAnchor, fallback) {
    if (!view.frames.length) return 0;
    if (srcAnchor != null) {
        const exact = srcToPlayIndex(view, srcAnchor);
        if (exact >= 0) return exact;
        for (let i = 0; i < view.frames.length; i++) {
            if (view.frames[i].srcIdx > srcAnchor) return i;
        }
        return view.frames.length - 1;
    }
    return Math.min(fallback, view.frames.length - 1);
}

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
        state.seekInFrameMs = 0;
    }
    refreshSelectionClasses();
    renderPreview();
    renderControls();
    reportPlayPosition();
}

function handleThumbContextMenu(idx, ev) {
    ev.preventDefault();
    if (!state.selectedSet.has(idx)) {
        state.selectedSet = new Set([idx]);
        refreshSelectionClasses();
    }
    // 点在已删帧上 → 恢复（保留选中，便于继续操作）；否则删除
    const deletedSet = new Set(state.view?.deleted || []);
    if (deletedSet.has(idx)) restoreSelected();
    else deleteSelected();
}

async function deleteSelected() {
    const view = state.view;
    const sel = [...state.selectedSet].filter(i => i < (view?.frameCount ?? 0));
    if (!sel.length) return;
    const deletedSet = new Set(view?.deleted || []);
    const toDelete = sel.filter(i => !deletedSet.has(i));
    if (!toDelete.length) {
        showHint(t('gif.editor.hint.alreadyDeleted'));
        return;
    }
    const skipped = sel.length - toDelete.length;
    state.selectedSet.clear();
    state.anchor = null;
    const cmd = toDelete.length === 1
        ? commands.deleteFrame(toDelete[0])
        : commands.deleteFrames(toDelete);
    await edit(cmd);
    if (skipped > 0) showHint(t('gif.editor.hint.deletedSkipped', {n: toDelete.length, skipped}));
}

/** 恢复选中集合中已删除的帧（时长从素材基线还原；保留选中便于继续操作）。 */
async function restoreSelected() {
    const view = state.view;
    const deletedSet = new Set(view?.deleted || []);
    const toRestore = [...state.selectedSet].filter(i => deletedSet.has(i));
    if (!toRestore.length) return;
    await edit(commands.restoreFrames(toRestore));
}

// ── 会话加载 ─────────────────────────────────────────────────────────────

async function loadSession(sessionId) {
    audioPlayback.clear();
    exportController.setSession(sessionId);
    const epoch = ++state.epoch;
    state.sessionId = sessionId;
    state.view = null;
    state.playIdx = 0;
    state.seekInFrameMs = 0;
    clearTimeout(state.posReportTimer);
    clearTimeout(state.seekDecodeTimer);
    state.selectedSet.clear();
    state.anchor = null;
    setPlaying(false);
    clearTimeout(state.playTimer);
    cancelAnimationFrame(state.progressRaf);
    state.progressRaf = 0;
    endSeekDrag(false);
    hideSeekPreview();
    state.thumbCache.clear();
    for (const bmp of state.previewCache.values()) bmp.close();
    state.previewCache.clear();
    state.previewBytes = 0;
    state.pendingBatches.length = 0;
    state.decodeKeys.clear();
    for (const [, el] of state.liveThumbs) el.remove();
    state.liveThumbs.clear();
    els.stripScroll.scrollLeft = 0;
    state.frameShownAt = performance.now();
    if (!sessionId) {
        showRecentView();
        return;
    }
    const view = await invoke('gif_get_session', {sessionId});
    if (epoch !== state.epoch) return;
    state.view = view;
    void audioPlayback.load(sessionId, view.audio?.enabled);
    renderAudioMix(view);
    state.durBySrc = effectiveDurMap(state.view);
    buildDurPrefix(state.view);
    renderSessionHeader();
    renderStopBanner(state.view.meta?.stopReason);
    measurePreviewWidth(state.view);
    layoutGeometry(state.view);
    // 草稿播放位置：按有效时长前缀和定位（编辑态已由后端恢复到 view）
    const draftPos = Number(view.draftPlayPosMs) || 0;
    if (draftPos > 0) {
        const loc = locateTime(state.durPrefix, draftPos);
        if (loc) {
            state.playIdx = loc.playIdx;
            state.seekInFrameMs = loc.inFrameMs;
            state.frameShownAt = performance.now() - state.seekInFrameMs;
        }
    }
    showEditorView();
    relayoutStrip();
    renderPreview();
    renderControls();
    if (state.playIdx > 0) syncPlayhead(currentPlaySrc(), false);
    if (view.draftStatus === 'discarded') showHint(t('gif.editor.draftDiscarded'), false);
    if (view.audioRecovered) showHint(t('gif.audio.recovered'), false);
    startPreload();
}

/** 标题与会话元信息（语言切换时重算）。 */
function renderSessionHeader() {
    els.title.textContent = t('gif.editor.title');
    const m = state.view?.meta;
    els.meta.textContent = m
        ? t('gif.editor.meta', {w: m.width, h: m.height, fps: m.fps, n: state.view.frameCount})
        : '';
}

function renderAudioMix(view) {
    const panel = $('audio-mix');
    if (!panel) return;
    for (let source = 0; source < 2; source++) {
        const row = $(`audio-mix-${source}`), setting = view.mix?.[source] || {gain: 1, muted: false};
        row.classList.toggle('hidden', !view.audio?.enabled?.[source]);
        row.querySelector('input[type="range"]').value = String(setting.gain * 100);
        row.querySelector('input[type="checkbox"]').checked = setting.muted;
        row.querySelector('output').textContent = `${Math.round(setting.gain * 100)}%`;
    }
    panel.classList.toggle('hidden', !view.audio?.enabled?.some(Boolean));
}
for (let source = 0; source < 2; source++) {
    const row = $(`audio-mix-${source}`);
    row?.addEventListener('input', () => { row.querySelector('output').textContent = `${row.querySelector('input[type="range"]').value}%`; });
    row?.addEventListener('change', () => edit({type: 'set_audio', source,
        gain: Number(row.querySelector('input[type="range"]').value) / 100,
        muted: row.querySelector('input[type="checkbox"]').checked}));
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

/** 瞬时提示（编辑失败等；2.5s 自动消失）。 */
function showHint(text, isError = true) {
    const el = els.hint;
    if (!el) return;
    el.textContent = text;
    el.classList.toggle('hint-error', isError);
    el.classList.remove('hidden');
    clearTimeout(state.hintTimer);
    state.hintTimer = setTimeout(() => el.classList.add('hidden'), 2500);
}

/** 非用户主动停止的会话横幅（预算到顶/过载/采集失败——用户需要知道
 *  录制为何提前结束）；用户主动停止不展示。 */
const STOP_REASON_KEYS = {
    budget_exhausted: 'gif.editor.stop.budget_exhausted',
    overload: 'gif.editor.stop.overload',
    capture_failed: 'gif.editor.stop.capture_failed',
};

function renderStopBanner(reason) {
    const banner = els.stopBanner;
    if (!banner) return;
    const key = STOP_REASON_KEYS[reason];
    if (!key) {
        banner.classList.add('hidden');
        return;
    }
    if (els.stopBannerText) els.stopBannerText.textContent = t(key);
    banner.classList.remove('hidden');
}

els.stopBannerClose?.addEventListener('click', () => {
    els.stopBanner?.classList.add('hidden');
});

async function edit(command) {
    if (!state.sessionId) return;
    setPlaying(false);
    const epoch = state.epoch;
    const srcAnchor = currentPlaySrc();
    let view;
    try {
        view = await invoke('gif_edit', {sessionId: state.sessionId, command});
    } catch (err) {
        console.error('[gif-editor] 编辑命令失败', err);
        showHint(t('gif.editor.hint.editFailed', {msg: normalizeError(err).message}));
        return;
    }
    if (epoch !== state.epoch || view.revision < (state.view?.revision ?? 0)) return;
    view = {...state.view, ...view};
    state.view = view;
    renderAudioMix(view);
    state.durBySrc = effectiveDurMap(view);
    buildDurPrefix(view);
    // 编辑 revision 变化：位置映射已变，关闭悬浮预览；在途解码经 epoch/
    // revision 双校验丢弃，不回填旧位置
    hideSeekPreview();
    state.playIdx = anchorPlayIndex(view, srcAnchor, state.playIdx);
    state.seekInFrameMs = 0;
    state.frameShownAt = performance.now();
    layoutGeometry(view);
    renderSessionHeader();
    // 已建帧槽全部重建（relayoutStrip 只补缺——不清空的话旧槽的
    // 时长标签/已删状态类不会随编辑刷新）
    for (const [, el] of state.liveThumbs) el.remove();
    state.liveThumbs.clear();
    relayoutStrip();
    renderPreview();
    renderControls();
}

/** 素材帧号 → 有效时长（含 hold/速度编辑后的真实播放值），帧标签用。 */
function effectiveDurMap(view) {
    return new Map((view.frames || []).map(f => [f.srcIdx, f.durMs]));
}

/** 播放时间轴前缀和（编辑后重建）。 */
function buildDurPrefix(view) {
    const prefix = [0];
    for (const f of view.frames || []) {
        prefix.push(prefix[prefix.length - 1] + f.durMs);
    }
    state.durPrefix = prefix;
}

/**
 * 播放进度（按时间轴，非帧号——帧时长不均时进度仍连续，缓解"某帧停顿
 * 很久画面不动"的卡顿感）。播放中含帧内已播时间；暂停/步进停在该帧起点；
 * 进度条定位后保留指针所在的帧内偏移（`seekInFrameMs`）。
 * 同步填充宽度、播放头位置、aria 值与时间码。
 */
function updatePlayProgress() {
    const view = state.view;
    const fill = els.playProgress;
    if (!fill) return;
    const total = state.durPrefix[state.durPrefix.length - 1] || 0;
    const disabled = !view || total <= 0 || !view.frames.length;
    els.seekBar?.setAttribute('aria-disabled', disabled ? 'true' : 'false');
    if (disabled) {
        fill.style.width = '0%';
        if (els.seekThumb) els.seekThumb.style.left = '0%';
        setSeekAria(0, 0);
        return;
    }
    const cur = Math.min(state.playIdx, view.frames.length - 1);
    const inFrame = state.playing
        ? Math.min(performance.now() - state.frameShownAt, view.frames[cur].durMs)
        : Math.min(state.seekInFrameMs, view.frames[cur].durMs);
    const timeMs = state.playing ? (performance.now() - state.playClockAt) % total : state.durPrefix[cur] + inFrame;
    audioPlayback.sync(timeMs, state.playing && !state.seekDragging, view.audioSpans, view.mix);
    const pct = Math.min(100, (timeMs / total) * 100);
    fill.style.width = `${pct}%`;
    if (els.seekThumb) els.seekThumb.style.left = `${pct}%`;
    setSeekAria(timeMs, total);
}

function setSeekAria(timeMs, totalMs) {
    const code = formatTimecode(timeMs);
    if (els.seekTime) els.seekTime.textContent = code;
    const bar = els.seekBar;
    if (!bar) return;
    bar.setAttribute('aria-valuemax', String(Math.round(totalMs)));
    bar.setAttribute('aria-valuenow', String(Math.round(timeMs)));
    bar.setAttribute('aria-valuetext', code);
}

function progressLoop() {
    if (!state.playing) return;
    updatePlayProgress();
    state.progressRaf = requestAnimationFrame(progressLoop);
}

// ── 进度定位条（0.28.23：单击/拖动/键盘/悬浮预览） ───────────────────────

/** 可定位判定（空素材/零时长禁用交互）。 */
function seekEnabled() {
    const total = state.durPrefix[state.durPrefix.length - 1] || 0;
    return Boolean(state.view && state.view.frames.length && total > 0);
}

/**
 * 定位到时间 t：按有效时长前缀和映射到播放序号 + 帧内偏移；只移动播放头
 * 与预览，不改编辑多选集合。拖动中 `renderPreview` 走降级绘制路径，
 * 全尺寸解码由 `scheduleSeekDecode` 防抖后对最新目标单独请求。
 */
function seekToTime(t) {
    const view = state.view;
    if (!view) return;
    const loc = locateTime(state.durPrefix, t);
    if (!loc) return;
    state.playIdx = loc.playIdx;
    state.seekInFrameMs = loc.inFrameMs;
    // frameShownAt 反推到"已展示 seekInFrameMs"——恢复播放时从指针位置续播
    state.frameShownAt = performance.now() - state.seekInFrameMs;
    if (state.playing) state.playClockAt = performance.now() - t;
    renderPreview();
    syncPlayhead(playToSrc(view, loc.playIdx), false);
    updatePlayProgress();
    scheduleSeekDecode();
}

/** 拖动中对最新目标防抖请求全尺寸解码（避免连续 scrub 打爆解码队列；
 *  解码按 srcIdx 入 previewCache——目标再变也只会留下正确的多余缓存，
 *  回流无需代次校验，缓存仍受字节预算约束）。 */
function scheduleSeekDecode() {
    clearTimeout(state.seekDecodeTimer);
    state.seekDecodeTimer = setTimeout(() => {
        if (!state.seekDragging) return;
        const srcIdx = currentPlaySrc();
        if (srcIdx == null || state.previewCache.has(srcIdx)) return;
        requestDecode(state.sessionId, [srcIdx], state.previewWidth, state.previewCache, true);
    }, SEEK_DECODE_DEBOUNCE_MS);
}

/** 结束拖动：恢复拖动前播放状态（提交位置后，上报播放头给草稿）。 */
function endSeekDrag(resume) {
    if (!state.seekDragging) return;
    state.seekDragging = false;
    clearTimeout(state.seekDecodeTimer);
    document.body.classList.remove('seek-dragging');
    els.seekBar?.classList.remove('seek-active');
    if (resume && state.seekWasPlaying) {
        setPlaying(true);
    } else {
        renderPreview(); // 拖动后按最终位置重绘（可能需要全尺寸解码）
    }
    reportPlayPosition();
}

/** 悬浮预览：显示目标帧缩略图与时间码，水平跟随指针、钳制在轨道内。
 *  缩略 miss 时低优先级补一张（入 thumbCache）；图在下次指针事件回填，
 *  鼠标完全静止则保持占位（时间码仍准确）。 */
function showSeekPreview(clientX) {
    const view = state.view;
    const total = state.durPrefix[state.durPrefix.length - 1] || 0;
    if (!view || !view.frames.length || total <= 0) return;
    const rect = els.seekBar.getBoundingClientRect();
    const ratio = pointerRatio(clientX, rect);
    const loc = locateTime(state.durPrefix, ratioToTime(ratio, total));
    if (!loc) return;
    const src = view.frames[loc.playIdx].srcIdx;
    state.seekPreviewSrc = src;
    const timeMs = state.durPrefix[loc.playIdx] + loc.inFrameMs;
    els.seekPreviewLabel.textContent = t('gif.editor.seek.preview', {
        frame: src,
        time: formatTimecode(timeMs),
    });
    els.seekPreviewImg.removeAttribute('src');
    const url = state.thumbCache.get(src);
    if (url) {
        els.seekPreviewImg.src = url;
    } else {
        requestDecode(state.sessionId, [src], 160, state.thumbCache);
    }
    const pw = els.seekPreview.offsetWidth || 120;
    els.seekPreview.style.left = `${clampPreviewLeft(clientX - rect.left, pw, rect.width)}px`;
    els.seekPreview.classList.remove('hidden');
}

function hideSeekPreview() {
    state.seekPreviewSrc = null;
    els.seekPreview?.classList.add('hidden');
    els.seekPreviewImg?.removeAttribute('src');
}

if (els.seekBar) {
    els.seekBar.addEventListener('pointerdown', ev => {
        if (!seekEnabled() || ev.button !== 0) return;
        ev.preventDefault();
        state.seekWasPlaying = state.playing;
        setPlaying(false);
        state.seekDragging = true;
        document.body.classList.add('seek-dragging');
        els.seekBar.classList.add('seek-active');
        els.seekBar.setPointerCapture(ev.pointerId);
        els.seekBar.focus({preventScroll: true}); // preventDefault 阻止了默认聚焦
        hideSeekPreview();
        seekToTime(ratioToTime(pointerRatio(ev.clientX, els.seekBar.getBoundingClientRect()),
            state.durPrefix[state.durPrefix.length - 1]));
    });
    els.seekBar.addEventListener('pointermove', ev => {
        if (state.seekDragging) {
            seekToTime(ratioToTime(pointerRatio(ev.clientX, els.seekBar.getBoundingClientRect()),
                state.durPrefix[state.durPrefix.length - 1]));
        } else if (ev.pointerType === 'mouse') {
            showSeekPreview(ev.clientX);
        }
    });
    els.seekBar.addEventListener('pointerup', ev => {
        if (!state.seekDragging) return;
        els.seekBar.releasePointerCapture?.(ev.pointerId);
        endSeekDrag(true);
    });
    els.seekBar.addEventListener('pointercancel', () => endSeekDrag(false));
    els.seekBar.addEventListener('pointerleave', () => {
        if (!state.seekDragging) hideSeekPreview();
    });
    els.seekBar.addEventListener('keydown', ev => {
        if (!seekEnabled()) return;
        const next = keyboardStep(state.playIdx, state.view.frames.length, ev.code);
        if (next == null) return;
        ev.preventDefault();
        setPlaying(false);
        state.seekInFrameMs = 0;
        state.playIdx = next;
        state.frameShownAt = performance.now();
        renderPreview();
        syncPlayhead(currentPlaySrc(), false);
        updatePlayProgress();
        reportPlayPosition();
    });
}

/** 帧时长输入框回显：跟随选中集（播放/步进时选中集即播放头所在帧）。
 *  单选显示该帧有效时长；多选留空（placeholder 提示批量）。
 *  输入框聚焦时跳过写入——播放不因输入暂停，不能清掉用户正在输入的值。 */
function syncDurInput() {
    const view = state.view;
    if (!view || !els.durInput) return;
    if (document.activeElement === els.durInput) return;
    const sel = [...state.selectedSet].filter(i => i < (view.frameCount ?? 0));
    if (sel.length === 1) {
        const f = (view.frames || []).find(x => x.srcIdx === sel[0]);
        els.durInput.value = f ? String(view.holds?.[sel[0]] ?? Math.round(f.durMs * (view.speed || 1))) : '';
        els.durInput.placeholder = '—';
    } else {
        els.durInput.value = '';
        els.durInput.placeholder = sel.length > 1 ? t('gif.editor.durPh.multi', {n: sel.length}) : t('gif.editor.durPh.select');
    }
}

function renderControls() {
    const view = state.view;
    if (!view) return;
    els.undo.disabled = !view.canUndo;
    els.redo.disabled = !view.canRedo;
    els.speed.value = String(view.speed);
    els.loop.value = view.loopCount == null ? '' : String(view.loopCount);
    syncDurInput();
    // 删除按钮：选中集合里没有可删帧时禁用（已删帧右键恢复）
    const sel = [...state.selectedSet].filter(i => i < (view.frameCount ?? 0));
    const deletedSet = new Set(view.deleted || []);
    const deletable = sel.some(i => !deletedSet.has(i));
    if (els.btnDelete) els.btnDelete.disabled = !deletable && sel.length > 0;
    // 导出按钮：0 有效帧不可导出
    els.exportBtn.disabled = !((view.frames || []).length);
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
    state.seekInFrameMs = 0;
    const src = currentPlaySrc();
    if (src != null) {
        state.selectedSet = new Set([src]);
        state.anchor = src;
    }
    renderPreview();
    if (src != null) syncPlayhead(src);
    syncDurInput();
    reportPlayPosition();
}

document.addEventListener('keydown', ev => {
    if (els.dialog.open) return;
    const tag = document.activeElement?.tagName;
    if (tag === 'INPUT' || tag === 'SELECT' || tag === 'TEXTAREA') return;
    // 进度条聚焦时方向键/Home/End 由其自身处理（定位语义，不循环、不改选中）
    if (document.activeElement === els.seekBar && ev.code !== 'Space' && ev.code !== 'Delete' && ev.code !== 'Backspace') return;
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

// 帧时长（ms）：作用于全部选中帧——批量单 undo；清空 = 恢复自动计算
const DUR_MIN_MS = 10, DUR_MAX_MS = 6000;
els.durInput?.addEventListener('change', () => {
    const sel = [...state.selectedSet].filter(i => i < (state.view?.frameCount ?? 0));
    if (!sel.length) {
        showHint(t('gif.editor.hint.selectFirst'));
        return;
    }
    const raw = parseInt(els.durInput.value, 10);
    if (!Number.isFinite(raw)) return;
    edit(commands.setHoldMany(sel, Math.max(DUR_MIN_MS, Math.min(DUR_MAX_MS, raw))));
});
els.durClear?.addEventListener('click', () => {
    const sel = [...state.selectedSet].filter(i => i < (state.view?.frameCount ?? 0));
    if (!sel.length) return;
    edit(commands.setHoldMany(sel, 0));
});

const exportController = createExportController();
document.getElementById('btn-export')?.addEventListener('click', () => setPlaying(false));
window.addEventListener('beforeunload', () => exportController.dispose());

// ── 视图切换 / 最近录制（0.28.23 §5.7.4）────────────────────────────────

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
    if (!state.sessionId || !state.view) return;
    const sessionId = state.sessionId;
    const send = () => {
        if (sessionId !== state.sessionId || !state.view) return;
        const offset = playbackOffset(state.view.frames[state.playIdx]?.durMs || 0,
            state.playing, state.frameShownAt, state.seekInFrameMs, performance.now());
        const pos = Math.round(timeAt(state.durPrefix, state.playIdx, offset));
        invoke('gif_set_play_position', {sessionId, posMs: pos}).catch(() => {});
    };
    clearTimeout(state.posReportTimer);
    if (immediate) send();
    else state.posReportTimer = setTimeout(send, 500);
}

window.addEventListener('pagehide', () => reportPlayPosition(true));

const sessionListener = listen(EVENTS.GIF_EDITOR_SESSION, event => {
    if (event.payload?.recordingSaved) showHint(t('gif.editor.recordingSaved'), false);
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
