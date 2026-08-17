//! 截图 overlay 输出动作（0.14.6 §4 拆分）。
//!
//! 包含：复制选区/全屏、钉图、保存、合成 PNG、取消截图。

import {ss} from './ss-state.js';
import * as annot from './annotation-engine.js';
import {cssPointToScreen, cssRectToBitmap} from './ss-selection-geometry.js';
import {
    hideScreenshotOverlay,
    imageEditorApplyToPin,
    imageEditorCancel,
    imageEditorCopy,
    imageEditorPin,
    imageEditorSave,
    screenshotCancel,
    screenshotCopy,
    screenshotCopyRegion,
    screenshotCopyRgba,
    screenshotPin,
    screenshotPinRegion,
    screenshotSave,
} from '../shared/api.js';
import {IMAGE_SOURCE} from './image-editor-session.js';
import {hideSelLoading, showTransientHint} from './ss-ocr.js';

/**
 * 清理画布视觉状态——在输出完成（copy/pin/save/cancel）后、窗口隐藏前调用。
 *
 * 动机：窗口复用时 eval resetState 与 win.show() 有竞态，旧画面会一闪而过。
 * 在完成时主动清空 canvas + 标注层，使窗口隐藏时画面已干净，下次唤起无残留。
 */
export function cleanupCanvasVisuals() {
    const {canvas, ctx, annotCanvas, annotCtx, toolbar, sizeHint, errorHint} = ss;
    // 清主 canvas
    if (canvas && ctx && canvas.width > 0) {
        ctx.clearRect(0, 0, canvas.width, canvas.height);
    }
    // 清标注 canvas
    if (annotCanvas && annotCtx) {
        annotCtx.clearRect(0, 0, annotCanvas.width, annotCanvas.height);
        annotCanvas.width = 0;
        annotCanvas.height = 0;
        annotCanvas.classList.add('hidden');
    }
    // 清标注引擎状态
    annot.clearAll();
    annot.clearOverlay();
    // 隐藏 UI 元素
    if (toolbar) toolbar.classList.add('hidden');
    if (sizeHint) sizeHint.classList.add('hidden');
}

export function ensureOutputReady() {
    const overlay = annot.getOverlay();
    const activeTranslation = ss.translationBusy && overlay && overlay.mode === 'translated';
    if (!ss.ocrBusy && !activeTranslation) return true;
    showTransientHint(activeTranslation ? '翻译尚未完成' : '识别尚未完成');
    return false;
}

export function doCopySelection() {
    if (!ss.selCss || ss.sent || !ensureOutputReady()) return;
    ss.sent = true;
    const hasAnnot = annot.hasAnnotations();
    // ⚠️ 临时打桩日志（0.19.14 性能排查用），收尾时清理
    const _t0 = performance.now();
    console.info('[screenshot] copy selection start', {hasAnnot, selW: ss.selCss.w, selH: ss.selCss.h});

    // 快路径：无标注 → 后端直接从 SESSION 裁剪 BGRA 写剪贴板
    if (!hasAnnot && ss.editorSession.canUseCaptureCropFastPath) {
        const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
        // 统一用 cssRectToBitmap 生成 bitmap rect，避免 X/Y scale 分歧
        const bmp = cssRectToBitmap(ss.selCss, meta);
        cleanupCanvasVisuals();
        screenshotCopyRegion(bmp.x, bmp.y, bmp.w, bmp.h)
            .then(() => console.info('[screenshot] copy 成功（快路径）', {ms: Math.round(performance.now() - _t0)}))
            .catch((err) => {
                console.error('[screenshot] copy 失败（快路径）', err);
                ss.errorHint.textContent = '截图保存失败：' + err;
                ss.errorHint.classList.remove('hidden');
                ss.sent = false;
            });
        return;
    }

    // P7: 有标注 copy → getImageData RGBA 直传，跳过 toBlob + PNG decode
    if (hasAnnot && ss.editorSession.canUseCaptureCropFastPath) {
        compositeSelectionRgba().then(({rgba, w, h}) => {
            cleanupCanvasVisuals();
            screenshotCopyRgba(rgba, w, h)
                .then(() => {
                    console.info('[screenshot] copy 成功（RGBA 直传）', {ms: Math.round(performance.now() - _t0)});
                    cleanupLongCapture();
                })
                .catch((err) => {
                    console.error('[screenshot] copy 失败（RGBA 直传）', err);
                    ss.errorHint.textContent = '截图保存失败：' + err;
                    ss.errorHint.classList.remove('hidden');
                    ss.sent = false;
                });
        }).catch((err) => {
            console.error('[screenshot] compositeSelectionRgba failed', err);
            ss.sent = false;
        });
        return;
    }

    compositeSelection((pngBytes) => {
        cleanupCanvasVisuals();
        outputEditorPng('copy', pngBytes)
            .then(() => {
                console.info('[screenshot] copy 成功（PNG 路径）', {ms: Math.round(performance.now() - _t0)});
                cleanupLongCapture();
            })
            .catch((err) => {
                console.error('[screenshot] copy 失败', err);
                ss.errorHint.textContent = '截图保存失败：' + err;
                ss.errorHint.classList.remove('hidden');
                ss.sent = false;
            });
    });
}

export function doCopyFullScreen() {
    if (ss.sent) return;
    ss.sent = true;
    // ⚠️ 临时打桩日志（0.19.14 性能排查用），收尾时清理
    const _t0 = performance.now();
    console.info('[screenshot] copy fullscreen start', {canvasW: ss.canvas.width, canvasH: ss.canvas.height});
    cleanupCanvasVisuals();
    screenshotCopyRegion(0, 0, ss.canvas.width, ss.canvas.height)
        .then(() => console.info('[screenshot] fullscreen copy 成功（快路径）', {ms: Math.round(performance.now() - _t0)}))
        .catch((err) => {
            console.error('[screenshot] fullscreen copy 失败', err);
            ss.errorHint.textContent = '截图保存失败：' + err;
            ss.errorHint.classList.remove('hidden');
            ss.sent = false;
        });
}

export function doPinSelection() {
    if (!ss.selCss || ss.sent || !ensureOutputReady()) return;
    ss.sent = true;
    const hasAnnot = annot.hasAnnotations();
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    // 统一用 cssPointToScreen 计算屏幕物理坐标
    const screenPos = ss.editorSession.canvasBacked
        ? {x: ss.editorSession.screenX || ss.scrollBandX, y: ss.editorSession.screenY || ss.scrollBandY}
        : cssPointToScreen(ss.selCss.x, ss.selCss.y, meta);
    const screenX = screenPos.x;
    const screenY = screenPos.y;
    // ⚠️ 临时打桩日志（0.19.14 性能排查用），收尾时清理
    const _t0 = performance.now();
    console.info('[screenshot] pin start', {hasAnnot, selW: ss.selCss.w, selH: ss.selCss.h});

    // 0.19.14 快路径：无标注 → 后端直接 crop BGRA + encode PNG + show_pin，
    // 跳过前端 toBlob + IPC PNG 往返（省 ~1200ms 全屏）
    if (!hasAnnot && ss.editorSession.canUseCaptureCropFastPath) {
        const bmp = cssRectToBitmap(ss.selCss, meta);
        // ⚠️ 临时诊断日志（跨 DPR 排查用），收尾时清理
        console.info('[screenshot] pin fast path', {
            selCss: ss.selCss,
            bmpRect: bmp,
            screenX, screenY,
            renderScale: meta.renderScaleX,
            dpr: window.devicePixelRatio,
        });
        cleanupCanvasVisuals();
        screenshotPinRegion(bmp.x, bmp.y, bmp.w, bmp.h, screenX, screenY)
            .then(() => console.info('[screenshot] pin 成功（快路径）', {ms: Math.round(performance.now() - _t0)}))
            .catch((err) => {
                console.error('[screenshot] pin 失败（快路径）', err);
                ss.sent = false;
            });
        return;
    }

    compositeSelection((pngBytes) => {
        cleanupCanvasVisuals();
        outputEditorPng('pin', pngBytes, screenX, screenY)
            .then(() => {
                console.info('[screenshot] pin 成功（PNG 路径）', {ms: Math.round(performance.now() - _t0)});
                cleanupLongCapture();
            })
            .catch((err) => {
                console.error('[screenshot] pin 失败', err);
                ss.sent = false;
            });
    });
}

export function doSaveSelection() {
    if (!ss.selCss || ss.sent || !ensureOutputReady()) return;
    ss.sent = true;
    compositeSelection((pngBytes) => {
        cleanupCanvasVisuals();
        outputEditorPng('save', pngBytes)
            .then(() => cleanupLongCapture())
            .catch((err) => {
                if (err !== '用户取消了保存') {
                    console.error('[screenshot] save 失败', err);
                }
                ss.sent = false;
            });
    });
}

/**
 * 合成选区（裁剪区 + 标注）为 PNG bytes。
 * 同时兼容旧 callback 调用和 Promise 调用；canvas-backed 来源直接使用会话底图。
 */
export function compositeSelection(callback) {
    const promise = compositeSelectionBytes();
    if (typeof callback === 'function') {
        promise.then((bytes) => {
            if (bytes) callback(bytes);
        });
    }
    return promise;
}

/** 0.18.3：导出合成函数，供「翻译并 pin」后台合成译文 PNG 用。 */
export {compositeSelectionBytes};

// M10 优化：复用合成 canvas，避免每次 compositeSelectionBytes / encodeImageDataPng 都 createElement
let _compositeCanvas = null;

/** M10 优化：获取复用的 canvas（尺寸不匹配时自动 resize + 清空） */
function getCompositeCanvas(w, h) {
    if (!_compositeCanvas) {
        _compositeCanvas = document.createElement('canvas');
    }
    if (_compositeCanvas.width !== w || _compositeCanvas.height !== h) {
        _compositeCanvas.width = w;
        _compositeCanvas.height = h;
    }
    const ctx = _compositeCanvas.getContext('2d');
    ctx.clearRect(0, 0, w, h);
    return _compositeCanvas;
}

/** 将 ImageData 编码为后端输出接口统一接收的 PNG 字节。 */
export async function encodeImageDataPng(imageData) {
    if (!imageData) return null;
    const canvas = getCompositeCanvas(imageData.width, imageData.height);
    canvas.getContext('2d').putImageData(imageData, 0, 0);
    const blob = await new Promise((resolve) => canvas.toBlob(resolve, 'image/png'));
    return blob ? new Uint8Array(await blob.arrayBuffer()) : null;
}

/** 统一分发已经编码好的截图输出，供普通截图和长截图复用。
 *  0.18.3：pin 动作支持 showTranslating 参数（控制 pin 窗口「翻译中」指示器）。 */
export async function outputScreenshotPng(action, pngBytes, screenX = 0, screenY = 0, showTranslating = false) {
    switch (action) {
        case 'pin':
            return screenshotPin(pngBytes, screenX, screenY, showTranslating);
        case 'save':
            return screenshotSave(pngBytes, null);
        case 'copy':
            return screenshotCopy(pngBytes);
        default:
            return undefined;
    }
}

/** 按编辑来源分派输出：截图保留原位 pin/来源标记，普通图片走用户输出适配器。 */
export async function outputEditorPng(action, pngBytes, screenX = 0, screenY = 0, showTranslating = false) {
    // 0.20.4：CLIPBOARD / HISTORY / PIN 都走 image_editor 后端路径
    // （finish_image_editor_session → hide_image_editor_window，不 cloak 窗口）。
    // 只有 SCREENSHOT / LONG_SCREENSHOT 走 screenshot 后端路径。
    if (ss.editorSession.source === IMAGE_SOURCE.SCREENSHOT
        || ss.editorSession.source === IMAGE_SOURCE.LONG_SCREENSHOT) {
        return outputScreenshotPng(action, pngBytes, screenX, screenY, showTranslating);
    }
    // 0.20.x：pin 来源编辑器的勾按钮（copy 动作）语义改为「替换回原 pin 窗口」。
    // 原窗口已关时后端自动重新 pin 到桌面。label 由后端 open_image_editor_from_pin 注入。
    if (ss.editorSession.source === IMAGE_SOURCE.PIN && action === 'copy') {
        const label = window.__blinkEditorSource?.label;
        return imageEditorApplyToPin(pngBytes, label);
    }
    switch (action) {
        case 'pin':
            return imageEditorPin(pngBytes, showTranslating);
        case 'save':
            return imageEditorSave(pngBytes, null);
        case 'copy':
            return imageEditorCopy(pngBytes);
        default:
            return undefined;
    }
}

async function compositeSelectionBytes() {
    if (!ss.selCss || (!ss.screenshot && !ss.editorSession.baseCanvas)) {
        console.error('[screenshot] compositeSelection: no selection or screenshot');
        ss.sent = false;
        return null;
    }
    const _t0 = performance.now();
    try {
        const sessionBase = ss.editorSession.baseCanvas;
        const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
// 统一用 cssRectToBitmap 生成 bitmap rect，快路径与合成路径输出一致
        const bmp = sessionBase ? {
            x: 0,
            y: 0,
            w: sessionBase.width,
            h: sessionBase.height
        } : cssRectToBitmap(ss.selCss, meta);
        const px = bmp.x;
        const py = bmp.y;
        const pw = bmp.w;
        const ph = bmp.h;

// 防御校验：输出尺寸必须 > 0
        if (pw <= 0 || ph <= 0) {
            throw new Error(`compositeSelectionBytes: invalid output dimensions { pw: ${pw}, ph: ${ph} }`);
        }

        const off = getCompositeCanvas(pw, ph);
        const offCtx = off.getContext('2d');
        if (sessionBase) {
            if (sessionBase.width <= 0 || sessionBase.height <= 0) {
                throw new Error(`compositeSelectionBytes: sessionBase has zero dimensions { w: ${sessionBase.width}, h: ${sessionBase.height} }`);
            }
            offCtx.drawImage(sessionBase, 0, 0);
        } else {
            if (!ss.screenshot || ss.screenshot.width <= 0 || ss.screenshot.height <= 0) {
                throw new Error(`compositeSelectionBytes: screenshot source has zero dimensions`);
            }
            offCtx.drawImage(ss.screenshot, px, py, pw, ph, 0, 0, pw, ph);
        }
        if (annot.hasAnnotations()) {
            if (!ss.annotCanvas || ss.annotCanvas.width <= 0 || ss.annotCanvas.height <= 0) {
                throw new Error(`compositeSelectionBytes: annotCanvas has zero dimensions { w: ${ss.annotCanvas?.width}, h: ${ss.annotCanvas?.height} }`);
            }
            offCtx.drawImage(ss.annotCanvas, 0, 0);
        }
        console.info('[screenshot] compositeSelectionBytes: sync draw done', {
            pw,
            ph,
            ms: Math.round(performance.now() - _t0)
        });
        const blob = await new Promise((resolve) => off.toBlob(resolve, 'image/png'));
        console.info('[screenshot] compositeSelectionBytes: toBlob done', {ms: Math.round(performance.now() - _t0)});
        if (!blob) {
            console.error('[screenshot] PNG 合成失败（blob=null）');
            ss.sent = false;
            return null;
        }
        const bytes = new Uint8Array(await blob.arrayBuffer());
        return bytes;
    } catch (e) {
        console.error('[screenshot] compositeSelection threw', e);
        ss.sent = false;
        return null;
    }
}

/** P7：合成选区为 raw RGBA bytes（用 getImageData 替代 toBlob，消除 PNG 编解码）。 */
async function compositeSelectionRgba() {
    if (!ss.selCss || (!ss.screenshot && !ss.editorSession.baseCanvas)) {
        throw new Error('compositeSelectionRgba: no selection or screenshot');
    }
    const sessionBase = ss.editorSession.baseCanvas;
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const bmp = sessionBase ? {
        x: 0,
        y: 0,
        w: sessionBase.width,
        h: sessionBase.height
    } : cssRectToBitmap(ss.selCss, meta);
    const pw = bmp.w;
    const ph = bmp.h;

    // 防御校验：输出尺寸必须 > 0
    if (pw <= 0 || ph <= 0) {
        throw new Error(`compositeSelectionRgba: invalid output dimensions { pw: ${pw}, ph: ${ph} }`);
    }

    const off = getCompositeCanvas(pw, ph);
    const offCtx = off.getContext('2d');
    if (sessionBase) {
        if (sessionBase.width <= 0 || sessionBase.height <= 0) {
            throw new Error(`compositeSelectionRgba: sessionBase has zero dimensions { w: ${sessionBase.width}, h: ${sessionBase.height} }`);
        }
        offCtx.drawImage(sessionBase, 0, 0);
    } else {
        if (!ss.screenshot || ss.screenshot.width <= 0 || ss.screenshot.height <= 0) {
            throw new Error(`compositeSelectionRgba: screenshot source has zero dimensions`);
        }
        offCtx.drawImage(ss.screenshot, bmp.x, bmp.y, pw, ph, 0, 0, pw, ph);
    }
    if (annot.hasAnnotations()) {
        if (!ss.annotCanvas || ss.annotCanvas.width <= 0 || ss.annotCanvas.height <= 0) {
            throw new Error(`compositeSelectionRgba: annotCanvas has zero dimensions { w: ${ss.annotCanvas?.width}, h: ${ss.annotCanvas?.height} }`);
        }
        offCtx.drawImage(ss.annotCanvas, 0, 0);
    }
    // getImageData 产生 RGBA，无需 toBlob PNG 编码
    const imageData = offCtx.getImageData(0, 0, pw, ph);
    const rgba = new Uint8Array(imageData.data.buffer);
    return {rgba, w: pw, h: ph};
}

function cleanupLongCapture() {
    if (ss.editorSession.source !== IMAGE_SOURCE.LONG_SCREENSHOT) return;
    if (!ss.scrollSession.active && !ss._imagePan) return;
    Promise.resolve(ss.scrollSession.exit(false))
        .catch((e) => console.warn('[screenshot] long capture cleanup failed', e));
}

/**
 * 独立合成译文 PNG：将 rawPng 解码到任务自己的离屏 canvas，
 * 在其上绘制显式传入的 translated overlay，编码 PNG。
 *
 * 不读取 ss.screenshot、ss.annotCanvas、ss.editorSession.baseCanvas。
 * 专供「翻译并 Pin」后台第二次合成使用，避免依赖已可能被清理的全局 canvas。
 *
 * @param {{ rawPng: Uint8Array, overlay: object|null, width: number, height: number }} opts
 * @returns {Promise<Uint8Array|null>} PNG bytes
 */
export async function composeTranslatedPinPng({rawPng, overlay, width, height}) {
    if (!rawPng) {
        throw new Error('composeTranslatedPinPng: rawPng is null');
    }
    if (!Number.isFinite(width) || !Number.isFinite(height) || width <= 0 || height <= 0) {
        throw new Error(`composeTranslatedPinPng: invalid dimensions { width: ${width}, height: ${height} }`);
    }

    const canvas = document.createElement('canvas');
    canvas.width = width;
    canvas.height = height;
    const ctx = canvas.getContext('2d');

    // 解码 rawPng 到离屏 canvas
    const blob = new Blob([rawPng], {type: 'image/png'});
    const bitmap = await createImageBitmap(blob);
    if (bitmap.width <= 0 || bitmap.height <= 0) {
        bitmap.close?.();
        throw new Error(`composeTranslatedPinPng: decoded bitmap has zero dimensions { w: ${bitmap.width}, h: ${bitmap.height} }`);
    }
    ctx.drawImage(bitmap, 0, 0, width, height);
    bitmap.close?.();

    // 在其上绘制显式传入的 translated overlay
    if (overlay) {
        annot.renderOverlaySnapshotTo(overlay, ctx, width, height);
    }

    // 编码 PNG
    const outBlob = await new Promise((resolve) => canvas.toBlob(resolve, 'image/png'));
    if (!outBlob) {
        throw new Error('composeTranslatedPinPng: toBlob returned null');
    }
    return new Uint8Array(await outBlob.arrayBuffer());
}

export function doCancel() {
    if (ss.cancelInProgress) {
        console.warn('[screenshot] doCancel: cancelInProgress still true, ignoring');
        return;
    }
    ss.cancelInProgress = true;
    setTimeout(() => {
        ss.cancelInProgress = false;
    }, 2000);
    console.info('[screenshot] cancel');
    const source = ss.editorSession.source;
    try {
        hideSelLoading();
    } catch (e) {
        console.warn('[screenshot] hideSelLoading failed', e);
    }
    // 0.15.7：长截图状态清理
    if (source === IMAGE_SOURCE.LONG_SCREENSHOT && (ss.scrollSession.active || ss._imagePan)) {
        // 异步清理，不阻塞 cancel；入口由长截图 session 唯一提供。
        Promise.resolve(ss.scrollSession.exit(false)).catch(() => {
        });
    }
    if (source === IMAGE_SOURCE.CLIPBOARD || source === IMAGE_SOURCE.HISTORY || source === IMAGE_SOURCE.PIN) {
        cleanupCanvasVisuals();
        imageEditorCancel().catch((e) => console.error('[image-editor] cancel 失败', e));
    } else if (ss.isAnnotating) {
        cleanupCanvasVisuals();
        screenshotCancel().catch((e) => console.error('[screenshot] screenshotCancel 失败', e));
    } else {
        cleanupCanvasVisuals();
        hideScreenshotOverlay().catch((e) => console.error('[screenshot] hideScreenshotOverlay 失败', e));
    }
}

/** 判断是否有主动召唤出的交互面板/输入态。 */
export function hasActivePanel() {
    if (document.getElementById('ocr-panel')) return true;
    if (document.querySelector('.text-annot-input')) return true;
    // 0.15.11：水印/文字配置移至 sub-panel，检查 sub-panel 是否可见
    const subPanel = document.getElementById('sub-panel');
    if (subPanel && !subPanel.classList.contains('hidden')) return true;
    const overlay = annot.getOverlay();
    if (overlay && overlay.mode !== null) return true;
    return false;
}
