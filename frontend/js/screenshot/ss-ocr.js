//! 截图 overlay OCR 面板 + 翻译 + UI helpers（0.14.6 §4 拆分）。
//!
//! 包含：
//! - UI helpers：showSelLoading / hideSelLoading / showTransientHint / updateOutputButtonsDisabled 等
//! - OCR actions：doIdentifySelection / doOverlayTranslate / doPanelToggle
//! - OCR core：_runOcrFresh / activateOverlay / requestOverlayTranslation / translateOverlayLines
//! - OCR panel：showOcrResult（面板 DOM 创建 + tab 切换 + 翻译 + 拖动）

import {ss} from './ss-state.js';
import {redrawAnnotFull} from './ss-draw.js';
import {applyFloatingUiScale, findDisplayCssAt} from './ss-display.js';
import {cssPointToScreen, cssRectToBitmap, uiScaleAtCss} from './ss-selection-geometry.js';
import {enterReadingMode} from './ss-reading.js';
import * as annot from './annotation-engine.js';
import {copyToClipboard, ocrImage, cancelOcrRequest, screenshotPinRefresh, translateLines, translateText,} from '../shared/api.js';
import {normalizeError} from '../shared/tauri.js';
import {cleanupCanvasVisuals, composeTranslatedPinPng} from './ss-output.js';

// ════════════════════════════════════════════════════════════
//  OCR Request Cancellation (Task 6)
// ════════════════════════════════════════════════════════════

/**
 * 取消当前 active OCR 请求（如果有）。
 *
 * 在以下节点调用：
 * - ESC
 * - 重选
 * - exitAnnotationMode
 * - reset session
 * - 新截图 session
 * - overlay hide
 * - prewarm 被替换
 *
 * 旧请求的 finally 只能清理自己的 loading/ocrBusy，
 * 不得修改新 session 的 DOM/overlay/translation/pin。
 */
export function cancelActiveOcr() {
    if (ss.activeOcrHandle) {
        const handle = ss.activeOcrHandle;
        ss.activeOcrHandle = null;
        handle.cancel().catch(() => {});
    }
    // 同时取消预热（如果有）
    if (ss.ocrPrewarm) {
        ss.ocrPrewarm = null;
    }
}

// ════════════════════════════════════════════════════════════
//  UI Helpers
// ════════════════════════════════════════════════════════════

/** 有效文本判断：空字符串和纯空白都视为未翻译。 */
export function hasText(value) {
    return typeof value === 'string' && value.trim().length > 0;
}

export function showSelLoading(text) {
    const el = document.getElementById('sel-loading');
    if (!el || !ss.selCss) return;
    const label = el.querySelector('.sel-loading-text');
    if (label) label.textContent = text;
    el.style.left = (ss.selCss.x + ss.selCss.w / 2) + 'px';
    el.style.top = (ss.selCss.y + ss.selCss.h / 2) + 'px';
    el.hidden = false;
}

export function hideSelLoading() {
    const el = document.getElementById('sel-loading');
    if (el) el.hidden = true;
}

/** 简易临时提示(选区附近,2 秒后自动消失)。0.19.16：DPI 适配。 */
export function showTransientHint(msg) {
    const {errorHint, selCss} = ss;
    errorHint.textContent = msg;
    errorHint.classList.remove('hidden');
    errorHint.style.background = 'rgba(50,50,50,0.85)';
    errorHint.style.left = '-9999px';
    errorHint.style.top = '-9999px';
    errorHint.style.transform = 'none';

    requestAnimationFrame(() => {
        if (selCss) {
            const MARGIN = 8;
            // 0.19.16：按选区中心所在屏计算 uiScale，保证提示文字物理尺寸跨屏一致
            const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
            const cx = selCss.x + selCss.w / 2;
            const cy = selCss.y + selCss.h / 2;
            const uiScale = uiScaleAtCss(cx, cy, meta);
            errorHint.style.transformOrigin = 'top left';
            errorHint.style.transform = `scale(${uiScale})`;
            const ew = errorHint.offsetWidth * uiScale;
            const eh = errorHint.offsetHeight * uiScale;
            const mon = findDisplayCssAt(cx, cy);
            let left = selCss.x + (selCss.w - ew) / 2;
            left = Math.max(mon.x + MARGIN, Math.min(left, mon.x + mon.w - ew - MARGIN));
            let top = selCss.y - eh - MARGIN;
            if (top < mon.y + MARGIN) {
                top = selCss.y + selCss.h + MARGIN;
            }
            errorHint.style.left = left + 'px';
            errorHint.style.top = top + 'px';
        } else {
            errorHint.style.left = '50%';
            errorHint.style.top = '50%';
            errorHint.style.transform = 'translate(-50%, -50%)';
        }
    });

    setTimeout(() => {
        errorHint.classList.add('hidden');
        errorHint.style.background = '';
        errorHint.style.transform = '';
    }, 2000);
}

export function updateOutputButtonsDisabled() {
    const overlay = annot.getOverlay();
    const disabled = ss.ocrBusy || (ss.translationBusy && overlay && overlay.mode === 'translated');
    ['btn-save', 'btn-pin', 'btn-copy'].forEach((id) => {
        const btn = document.getElementById(id);
        if (btn) btn.disabled = disabled;
    });
}

/** 工具栏「识别」/「翻译」按钮高亮态：跟随面板当前 tab。 */
export function updateToolbarButtonActive() {
    const panel = document.getElementById('ocr-panel');
    const btnOcr = document.getElementById('btn-ocr');
    const btnTr = document.getElementById('btn-translate');
    if (!panel) {
        if (btnOcr) btnOcr.classList.remove('active');
        if (btnTr) btnTr.classList.remove('active');
        return;
    }
    const tabTranslated = panel.querySelector('.ocr-tab[data-tab="translated"]');
    const isTranslatedActive = tabTranslated && tabTranslated.classList.contains('active');
    if (btnOcr) btnOcr.classList.toggle('active', !isTranslatedActive);
    if (btnTr) btnTr.classList.toggle('active', isTranslatedActive);
}

/** 把 overlay 里已经翻译好的 line.dstText 回填到面板译文 textarea。 */
export function syncPanelTranslatedFromOverlay() {
    const overlay = annot.getOverlay();
    const translatedReady = !!(overlay && overlay.lines.length > 0 && overlay.lines.every((line) => hasText(line.dstText)));
    const panel = document.getElementById('ocr-panel');
    if (!translatedReady || !panel) return;
    const translatedTa = panel.querySelector('#ocr-textarea-translated');
    if (!translatedTa || translatedTa.getAttribute('data-loading') === 'true') return;
    translatedTa.value = overlay.lines.map((line) => line.dstText).join('\n');
    translatedTa.removeAttribute('data-stale');
    const tab = panel.querySelector('.ocr-tab[data-tab="translated"]');
    if (tab) tab.removeAttribute('data-stale');
}

/** 兼容包装：同步工具栏按钮 + 面板译文。 */
export function updateOverlayButtonsActive() {
    updateToolbarButtonActive();
    syncPanelTranslatedFromOverlay();
}

function tracing_debug(msg, extra) {
    console.info('[screenshot] ' + msg, extra || '');
}

// ════════════════════════════════════════════════════════════
//  OCR Actions
// ════════════════════════════════════════════════════════════

/**
 * 点[识别]——OCR → 面板（原文 tab），图上不嵌字。
 * 首次点 → OCR → 面板展开；面板已开 + 原文 tab → 关闭面板；
 * 面板已开 + 译文 tab → 切到原文 tab + 清 overlay + 隐藏 adv。
 */
export function doIdentifySelection() {
    if (!ss.selCss) return;
    const existingPanel = document.getElementById('ocr-panel');
    if (existingPanel && ss.ocrResultCache) {
        const tabSource = existingPanel.querySelector('.ocr-tab[data-tab="source"]');
        const isSourceActive = tabSource && tabSource.classList.contains('active');
        if (isSourceActive) {
            existingPanel.remove();
            annot.setOverlayMode(null);
            redrawAnnotFull();
            updateToolbarButtonActive();
        } else {
            annot.setOverlayMode(null);
            redrawAnnotFull();
            const adv = existingPanel.querySelector('.ocr-panel-adv');
            if (adv) adv.classList.add('hidden');
            if (tabSource) tabSource.click();
        }
        return;
    }
    ss.ocrBusy = true;
    updateOutputButtonsDisabled();
    showSelLoading('识别中…');
    const revision = ss.selectionRevision;
    const onResult = (result) => {
        if (revision !== ss.selectionRevision) return;
        activateOverlay(result, {
            showOverlay: false,
            panelTab: 'source',
            openPanel: true,
            autoTranslate: false,
        });
        ss.ocrBusy = false;
        updateOutputButtonsDisabled();
        hideSelLoading();
    };
    if (ss.ocrPrewarm) {
        console.debug('[screenshot] doIdentify 走预热缓存');
        ss.ocrPrewarm.then((result) => {
            if (revision !== ss.selectionRevision) return;
            if (result) {
                onResult(result);
                return;
            }
            _runOcrFresh({kind: 'identify', revision});
        }).catch((err) => {
            if (revision !== ss.selectionRevision) return;
            console.error('[screenshot] OCR 预热 Promise 异常', err);
            ss.ocrBusy = false;
            updateOutputButtonsDisabled();
            hideSelLoading();
            showTransientHint('识别失败');
        });
        return;
    }
    _runOcrFresh({kind: 'identify', revision});
}

/**
 * 点[翻译]——OCR + 翻译 → 面板（译文 tab）+ overlay 嵌译文。
 */
export function doOverlayTranslate() {
    if (!ss.selCss) return;
    const existingPanel = document.getElementById('ocr-panel');
    if (existingPanel && ss.ocrResultCache) {
        const tabTranslated = existingPanel.querySelector('.ocr-tab[data-tab="translated"]');
        const isTranslatedActive = tabTranslated && tabTranslated.classList.contains('active');
        if (isTranslatedActive) {
            existingPanel.remove();
            updateToolbarButtonActive();
        } else {
            const overlay = annot.getOverlay();
            if (overlay && overlay.mode !== 'translated') {
                annot.setOverlayMode('translated');
                redrawAnnotFull();
            }
            const adv = existingPanel.querySelector('.ocr-panel-adv');
            if (adv) adv.classList.remove('hidden');
            if (tabTranslated) tabTranslated.click();
        }
        return;
    }
    ss.ocrBusy = true;
    updateOutputButtonsDisabled();
    showSelLoading('识别中…');
    const revision = ss.selectionRevision;
    const onResult = (result) => {
        if (revision !== ss.selectionRevision) return;
        activateOverlay(result, {
            showOverlay: true,
            panelTab: 'translated',
            openPanel: true,
            autoTranslate: true,
        });
        ss.ocrBusy = false;
        updateOutputButtonsDisabled();
    };
    if (ss.ocrPrewarm) {
        console.debug('[screenshot] doTranslate 走预热缓存');
        ss.ocrPrewarm.then((result) => {
            if (revision !== ss.selectionRevision) return;
            if (result) {
                onResult(result);
                return;
            }
            _runOcrFresh({kind: 'translate', revision});
        }).catch((err) => {
            if (revision !== ss.selectionRevision) return;
            console.error('[screenshot] OCR 预热 Promise 异常', err);
            ss.ocrBusy = false;
            updateOutputButtonsDisabled();
            hideSelLoading();
            showTransientHint('识别失败');
        });
        return;
    }
    _runOcrFresh({kind: 'translate', revision});
}

// 保留旧名字作为别名
export const doOcrSelection = doIdentifySelection;
export const doTranslateSelection = doOverlayTranslate;

/**
 * 点[翻译并 pin]——立即 pin 原图 + 后台翻译 + 翻译完原地替换。
 * 0.19.16：重写为 job-local 合成链路，不再依赖全局 annotCanvas 做第二次合成。
 *
 * 核心改进：
 * - 启动时保存 editor epoch；
 * - 第一次 Pin 前生成 job-local rawPng（底图 + 标注，排除翻译 overlay）；
 * - 第二次合成使用独立 composeTranslatedPinPng，不读取 ss.annotCanvas；
 * - finally 中仅当 epoch 仍匹配时才清理 canvas 和 pending 状态。
 *
 * 时序：
 * 1. 合成原图 PNG（不嵌译文）→ screenshotPin(showTranslating=true) → 关 overlay
 * 2. 后台：OCR（命中预热或 fresh）→ translateLines → composeTranslatedPinPng → screenshotPinRefresh
 * 3. 翻译失败/超时：隐藏指示器，pin 保持原图
 */
export async function doTranslateAndPin() {
    if (!ss.selCss || ss.sent) return;
    if (ss._translateAndPinPending) return;

    ss._translateAndPinPending = true;
    const jobEpoch = ss.editorSession.epoch;
    let rawPng = null;
    let pinned = false;
    let jobWidth = 0;
    let jobHeight = 0;

    try {
        // ── Step 1: 立即 pin 原图 ──
        const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
        const screenPos = ss.editorSession.canvasBacked
            ? {x: ss.editorSession.screenX || ss.scrollBandX, y: ss.editorSession.screenY || ss.scrollBandY}
            : cssPointToScreen(ss.selCss.x, ss.selCss.y, meta);
        const screenX = screenPos.x;
        const screenY = screenPos.y;

        // 临时关闭 overlay mode 合成原图（不嵌译文层）
        const savedMode = annot.getOverlay()?.mode || null;
        if (savedMode) annot.setOverlayMode(null);

        rawPng = await ss._compositeSelection();

        // 恢复 overlay mode（后台翻译时要用来画译文）
        if (savedMode) annot.setOverlayMode(savedMode);

        if (!rawPng) {
            showTransientHint('合成截图失败');
            return;
        }

        // 记录合成尺寸供第二次 job-local 合成使用
        if (ss.editorSession.canvasBacked) {
            jobWidth = ss.editorSession.baseCanvas.width;
            jobHeight = ss.editorSession.baseCanvas.height;
        } else {
            const bmp = cssRectToBitmap(ss.selCss, meta);
            jobWidth = bmp.w;
            jobHeight = bmp.h;
        }

        // pin 原图 + 显示翻译中指示器（screenshot_pin 内部会关 overlay）
        await ss._outputEditorPng('pin', rawPng, screenX, screenY, true);
        pinned = true;

        // ── Step 2: 后台 OCR + 翻译 + 合成 + 替换 ──

        // 2a. 获取 OCR 结果（优先已有 overlay → 预热 → fresh）
        let ocrResult = null;
        const existingOverlay = annot.getOverlay();
        if (existingOverlay && existingOverlay.lines.length > 0 && ss.ocrResultCache) {
            ocrResult = ss.ocrResultCache;
        }

        if (!ocrResult && ss.ocrPrewarm) {
            try {
                ocrResult = await ss.ocrPrewarm;
            } catch (e) {
                console.warn('[screenshot] translateAndPin: OCR 预热失败', e);
            }
        }

        if (!ocrResult) {
            // Fresh OCR：复用已合成的原图 PNG
            // Task 6: 使用 handle 接口，支持取消
            const handle = ocrImage(rawPng, ss.editorSession.epoch, ss.selectionRevision);
            ss.activeOcrHandle = handle;
            try {
                const {result} = await handle.promise;
                ocrResult = result;
            } finally {
                if (ss.activeOcrHandle === handle) {
                    ss.activeOcrHandle = null;
                }
            }
        }

        if (!ocrResult || !ocrResult.lines || ocrResult.lines.length === 0) {
            // 未识别到文字：隐藏指示器，保持原图
            await screenshotPinRefresh(rawPng, false).catch(() => {
            });
            return;
        }

        const lines = ocrResult.lines.filter(
            (ln) => ln && ln.text && ln.rect && ln.rect.w > 0 && ln.rect.h > 0
        );
        if (lines.length === 0) {
            await screenshotPinRefresh(rawPng, false).catch(() => {
            });
            return;
        }

        // 2b. 构造任务局部 overlay（最终 Pin 合成以此为准）
        const jobOverlay = {
            lines: lines.map((ln) => ({
                rect: {x: ln.rect.x, y: ln.rect.y, w: ln.rect.w, h: ln.rect.h},
                srcText: ln.text,
                dstText: null,
                bgColor: null,
                inkColor: null,
            })),
            mode: 'translated',
            bgStrategy: 'average',
            fontScale: 1.0,
            showOriginal: false,
            translationTargetLang: null,
        };

        // 同步更新当前 annot overlay（如果 UI 尚未被重置）
        if (ss.editorSession.epoch === jobEpoch) {
            annot.setOverlay({
                lines: jobOverlay.lines.map((l) => ({...l})),
                mode: 'translated',
            });
            ss.ocrResultCache = ocrResult;
        }

        // 2c. 翻译
        const srcs = lines.map((ln) => ln.text);
        let translations;
        try {
            translations = await translateLines(srcs, null);
        } catch (e) {
            console.warn('[screenshot] translateAndPin: translateLines 失败，降级逐行', e);
            translations = [];
            for (let i = 0; i < srcs.length; i++) {
                try {
                    translations.push(await translateText(srcs[i], null));
                } catch (_) {
                    translations.push(srcs[i]);
                }
            }
        }

        // 2d. 回填译文到 job-local overlay
        for (let i = 0; i < Math.min(translations.length, jobOverlay.lines.length); i++) {
            jobOverlay.lines[i].dstText = translations[i];
        }

        // 同步更新 UI（如果当前 epoch 仍匹配）
        if (ss.editorSession.epoch === jobEpoch) {
            annot.setOverlayTranslations(translations, null);
            redrawAnnotFull();
        }

        // 2e. 用 job-local 合成链路生成译文 PNG（不依赖 ss.annotCanvas）
        const translatedPng = await composeTranslatedPinPng({
            rawPng,
            overlay: jobOverlay,
            width: jobWidth,
            height: jobHeight,
        });

        if (!translatedPng) {
            // 合成失败：隐藏指示器，保持原图
            await screenshotPinRefresh(rawPng, false).catch(() => {
            });
            return;
        }

        // 2f. 原地替换 pin 图片 + 隐藏指示器
        await screenshotPinRefresh(translatedPng, false);

    } catch (e) {
        console.error('[screenshot] translateAndPin 失败', e);
        // 翻译失败/超时：隐藏指示器，pin 保持原图
        if (pinned && rawPng) {
            await screenshotPinRefresh(rawPng, false).catch(() => {
            });
        }
    } finally {
        // 只有当前 editor epoch 仍等于任务启动 epoch 时才清理
        if (ss.editorSession.epoch === jobEpoch) {
            ss._translateAndPinPending = false;
            cleanupCanvasVisuals();
        } else {
            // 新会话已开始，旧任务不清理新 canvas，也不改 pending 状态
            console.debug('[screenshot] translateAndPin: epoch mismatch, skipping cleanup', {
                jobEpoch, currentEpoch: ss.editorSession.epoch,
            });
        }
    }
}

/** E 键召唤/关闭面板抽屉 */
export function doPanelToggle() {
    const panel = document.getElementById('ocr-panel');
    if (panel) {
        panel.remove();
        updateToolbarButtonActive();
        return;
    }
    if (ss.ocrResultCache) {
        showOcrResult(ss.ocrResultCache);
    } else {
        doIdentifySelection();
    }
}

// ════════════════════════════════════════════════════════════
//  OCR Core
// ════════════════════════════════════════════════════════════

/** 走完整合成 → OCR → 展示的正常路径(预热未开或失败时兜底)。 */
function _runOcrFresh(opts = {}) {
    const kind = opts.kind || 'identify';
    const revision = opts.revision ?? ss.selectionRevision;
    showSelLoading('识别中…');
    if (typeof ss._compositeSelection !== 'function') return;
    ss._compositeSelection((pngBytes) => {
        // Task 6: 同步产生 requestId，立即暴露 cancel()
        const handle = ocrImage(pngBytes, ss.editorSession.epoch, revision);
        ss.activeOcrHandle = handle;

        handle.promise
            .then(({result}) => {
                // 旧请求的 finally 会清理 activeOcrHandle
                if (revision !== ss.selectionRevision) {
                    console.debug('[screenshot] 丢弃旧选区 OCR 结果', {revision, current: ss.selectionRevision});
                    return;
                }
                if (kind === 'translate') {
                    showSelLoading('翻译中…');
                    activateOverlay(result, {
                        showOverlay: true,
                        panelTab: 'translated',
                        openPanel: true,
                        autoTranslate: true,
                    });
                } else {
                    hideSelLoading();
                    activateOverlay(result, {
                        showOverlay: false,
                        panelTab: 'source',
                        openPanel: true,
                        autoTranslate: false,
                    });
                }
            })
            .catch((rawErr) => {
                const err = normalizeError(rawErr);
                if (revision === ss.selectionRevision) {
                    showTransientHint(err.retryable ? '识别失败，可重试' : '识别失败');
                }
                hideSelLoading();
                console.error(`[screenshot] ocr 失败 [${err.code}] ${err.message}`);
            })
            .finally(() => {
                // 只有当当前 handle 仍是自己时才清理
                if (ss.activeOcrHandle === handle) {
                    ss.activeOcrHandle = null;
                }
                if (revision === ss.selectionRevision) {
                    ss.ocrBusy = false;
                    updateOutputButtonsDisabled();
                }
            });
    });
}

/** 把 OCR 结果落地为 overlayLayer + reading + 面板。 */
function activateOverlay(result, opts = {}) {
    const lines = (result && Array.isArray(result.lines)) ? result.lines : [];
    const nonEmpty = lines.filter((ln) => ln && ln.text && ln.rect && ln.rect.w > 0 && ln.rect.h > 0);
    if (nonEmpty.length === 0) {
        showTransientHint('未识别到文字');
        return false;
    }
    const mode = opts.showOverlay ? (opts.panelTab === 'translated' ? 'translated' : 'source') : null;
    annot.setOverlay({
        lines: nonEmpty.map((ln) => ({
            rect: {x: ln.rect.x, y: ln.rect.y, w: ln.rect.w, h: ln.rect.h},
            srcText: ln.text,
        })),
        mode,
    });
    ss.ocrResultCache = result;
    if (result && Array.isArray(result.words) && result.words.length > 0) {
        enterReadingMode(result);
    }
    redrawAnnotFull();
    updateOverlayButtonsActive();
    if (opts.openPanel) {
        showOcrResult(result, {tab: opts.panelTab || 'source', showOverlay: opts.showOverlay});
    }
    if (opts.autoTranslate) {
        requestOverlayTranslation();
    }
    return true;
}

function requestOverlayTranslation(targetLang) {
    const revision = ++ss.translationRevision;
    ss.translationBusy = true;
    updateOutputButtonsDisabled();
    // 互斥：doTranslate 路径已激活 canvas loading (Loading B) 时，
    // 跳过 DOM spinner (Loading A)；仅 0.15 redo 路径（无 canvas loading）才用 DOM spinner。
    const useOverlayLoading = annot.isOverlayLoading();
    if (!useOverlayLoading) showSelLoading('翻译中…');
    translateOverlayLines(targetLang, revision)
        .catch((e) => {
            if (revision !== ss.translationRevision) return;
            showTransientHint('翻译失败');
            console.error('[screenshot] overlay translate 失败', e);
        })
        .finally(() => {
            if (revision !== ss.translationRevision) return;
            ss.translationBusy = false;
            updateOutputButtonsDisabled();
            if (!useOverlayLoading) hideSelLoading();
        });
}

async function translateOverlayLines(targetLang, revision = ++ss.translationRevision) {
    const selectionAtStart = ss.selectionRevision;
    const current = annot.getOverlay();
    if (!current || current.lines.length === 0) return;
    const srcs = current.lines.map((l) => hasText(l.dstText) ? '' : (l.srcText || ''));
    const needCount = srcs.filter((s) => hasText(s)).length;
    if (needCount === 0) return;

    const started = performance.now();
    let dsts;
    try {
        dsts = await translateLines(srcs, targetLang || null);
    } catch (e) {
        const err = normalizeError(e);
        console.warn(`[screenshot] translateLines 失败 [${err.code}],降级到逐行单调`);
        dsts = [];
        for (let i = 0; i < srcs.length; i++) {
            if (!hasText(srcs[i])) {
                dsts.push('');
                continue;
            }
            try {
                dsts.push(await translateText(srcs[i], targetLang || null));
            } catch (_) {
                dsts.push(srcs[i]);
            }
        }
    }
    if (selectionAtStart !== ss.selectionRevision || revision !== ss.translationRevision) {
        console.debug('[screenshot] 丢弃过期翻译结果', {revision, current: ss.translationRevision});
        return;
    }
    const latest = annot.getOverlay();
    if (!latest || latest.lines.length !== current.lines.length) return;
    const merged = latest.lines.map((l, i) => hasText(l.dstText) ? l.dstText : (dsts[i] || l.srcText));
    annot.setOverlayTranslations(merged, targetLang || null);
    redrawAnnotFull();
    updateOverlayButtonsActive();
    tracing_debug('translateOverlayLines 完成', {lines: needCount, ms: Math.round(performance.now() - started)});
}

// ════════════════════════════════════════════════════════════
//  OCR Panel (showOcrResult)
// ════════════════════════════════════════════════════════════

export function showOcrResult(result, options = {}) {
    const old = document.getElementById('ocr-panel');
    if (old) old.remove();

    const text = (result && result.text) || '';
    const initialText = text || '（未识别到文字）';
    const initialTab = options.tab === 'translated' ? 'translated' : 'source';
    const overlayForLabel = annot.getOverlay();
    const hasTranslation = overlayForLabel && overlayForLabel.lines.length > 0
        && overlayForLabel.lines.every((l) => hasText(l.dstText));
    const translateLabel = hasTranslation ? '重新翻译' : '翻译';

    const panel = document.createElement('div');
    panel.id = 'ocr-panel';
    panel.className = 'ocr-panel';
    panel.innerHTML = `
    <div class="ocr-panel-header">
      <div class="ocr-tabs">
        <button class="ocr-tab ${initialTab === 'source' ? 'active' : ''}" data-tab="source">原文</button>
        <button class="ocr-tab ${initialTab === 'translated' ? 'active' : ''}" data-tab="translated">译文 <span class="stale-dot" aria-hidden="true"></span></button>
      </div>
      <div class="ocr-panel-actions">
        <button id="ocr-copy" class="tool-btn tool-btn-primary" ${text ? '' : 'disabled'}>复制</button>
        <button id="ocr-translate" class="tool-btn" ${text ? '' : 'disabled'} title="翻译当前文本">${translateLabel}</button>
      </div>
      <button id="ocr-close" class="tool-btn" title="关闭面板">✕</button>
    </div>
    <textarea id="ocr-textarea-source" class="ocr-panel-textarea" spellcheck="false"></textarea>
    <textarea id="ocr-textarea-translated" class="ocr-panel-textarea" spellcheck="false" hidden placeholder="点击翻译按钮生成译文"></textarea>
    <div class="ocr-panel-adv">
      <label class="ocr-adv-item">
        <span>嵌图背景</span>
        <select id="ocr-bg-strategy">
          <option value="average">平均色</option>
          <option value="blur">高斯模糊</option>
          <option value="solid">半透明板</option>
        </select>
      </label>
      <label class="ocr-adv-item">
        <span>字号</span>
        <input id="ocr-font-scale" type="range" min="60" max="140" step="10" value="100" title="嵌图字号缩放(60%-140%)" />
        <span id="ocr-font-scale-val">100%</span>
      </label>
    </div>
  `;
    document.body.appendChild(panel);

    if (initialTab !== 'translated') {
        const advSection = panel.querySelector('.ocr-panel-adv');
        if (advSection) advSection.classList.add('hidden');
    }

    const sourceTa = panel.querySelector('#ocr-textarea-source');
    const translatedTa = panel.querySelector('#ocr-textarea-translated');
    const tabSource = panel.querySelector('.ocr-tab[data-tab="source"]');
    const tabTranslated = panel.querySelector('.ocr-tab[data-tab="translated"]');
    const translateBtn = panel.querySelector('#ocr-translate');
    const copyBtn = panel.querySelector('#ocr-copy');
    const bgStrategySelect = panel.querySelector('#ocr-bg-strategy');
    const fontScaleInput = panel.querySelector('#ocr-font-scale');
    const fontScaleVal = panel.querySelector('#ocr-font-scale-val');

    sourceTa.value = initialText;

    // 面板尺寸自适应
    const PANEL_CHROME = 90;
    const TA_MIN = 200, TA_MAX = 390;
    const lineH = 22.4;
    const CHARS_PER_LINE = 40;
    let textLines = 1;
    for (const line of String(text || '').split('\n')) {
        textLines += Math.max(1, Math.ceil((line.length + 1) / CHARS_PER_LINE) || 1);
        if (textLines > 40) break;
    }
    const taH = Math.max(TA_MIN, Math.min(TA_MAX, textLines * lineH));
    panel.style.height = (taH + PANEL_CHROME) + 'px';

    // 抽屉锚定触发按钮
    const MARGIN = 8;
    const mon = findDisplayCssAt(ss.selCss.x + ss.selCss.w / 2, ss.selCss.y + ss.selCss.h / 2);
    // 应用与工具栏一致的 UI scale
    const uiScale = applyFloatingUiScale(panel);
    const pw = panel.offsetWidth * uiScale;
    const ph = panel.offsetHeight * uiScale;
    const anchorBtn = document.getElementById('btn-ocr');
    const anchorRect = anchorBtn ? anchorBtn.getBoundingClientRect() : ss.toolbar.getBoundingClientRect();

    let left = anchorRect.left;
    if (left + pw > mon.x + mon.w - MARGIN) left = mon.x + mon.w - MARGIN - pw;
    left = Math.max(mon.x + MARGIN, left);

    let top = anchorRect.bottom + 4;
    if (top + ph > mon.y + mon.h - MARGIN) top = anchorRect.top - ph - 4;
    if (top < mon.y + MARGIN) top = Math.max(mon.y + MARGIN, mon.y + mon.h - MARGIN - ph);
    panel.style.left = left + 'px';
    panel.style.top = top + 'px';

    panel.addEventListener('mousedown', (e) => e.stopPropagation());
    document.getElementById('ocr-close').addEventListener('click', () => {
        panel.remove();
        updateToolbarButtonActive();
    });

    // 面板拖动
    const header = panel.querySelector('.ocr-panel-header');
    if (header) {
        let dragging = false;
        let offsetX = 0, offsetY = 0;
        header.addEventListener('mousedown', (e) => {
            if (e.target.closest('#ocr-close')) return;
            if (e.button !== 0) return;
            e.preventDefault();
            e.stopPropagation();
            dragging = true;
            const rect = panel.getBoundingClientRect();
            offsetX = e.clientX - rect.left;
            offsetY = e.clientY - rect.top;
            document.body.style.cursor = 'grabbing';
        });
        const onMove = (e) => {
            if (!dragging) return;
            if (!document.body.contains(panel)) {
                dragging = false;
                document.removeEventListener('mousemove', onMove);
                document.removeEventListener('mouseup', onUp);
                document.body.style.cursor = '';
                return;
            }
            const pwl = panel.offsetWidth * uiScale;
            const phl = panel.offsetHeight * uiScale;
            const monMove = findDisplayCssAt(e.clientX, e.clientY);
            let nl = e.clientX - offsetX;
            let nt = e.clientY - offsetY;
            nl = Math.max(monMove.x + MARGIN, Math.min(nl, monMove.x + monMove.w - pwl - MARGIN));
            nt = Math.max(monMove.y + MARGIN, Math.min(nt, monMove.y + monMove.h - phl - MARGIN));
            panel.style.left = nl + 'px';
            panel.style.top = nt + 'px';
        };
        const onUp = () => {
            if (!dragging) return;
            dragging = false;
            document.body.style.cursor = '';
        };
        document.addEventListener('mousemove', onMove);
        document.addEventListener('mouseup', onUp);
        document.getElementById('ocr-close').addEventListener('click', () => {
            document.removeEventListener('mousemove', onMove);
            document.removeEventListener('mouseup', onUp);
        });
    }

    // Tab 切换
    const showTab = (name) => {
        const isSource = name === 'source';
        tabSource.classList.toggle('active', isSource);
        tabTranslated.classList.toggle('active', !isSource);
        sourceTa.hidden = !isSource;
        translatedTa.hidden = isSource;
        (isSource ? sourceTa : translatedTa).focus();
        if (!isSource) {
            const overlay = annot.getOverlay();
            if (overlay && overlay.mode !== 'translated') {
                annot.setOverlayMode('translated');
                redrawAnnotFull();
            }
            const needsTranslation = overlay && overlay.lines.length > 0
                && overlay.lines.every((l) => !hasText(l.dstText));
            if (needsTranslation) {
                doTranslate();
            }
        } else {
            const overlay = annot.getOverlay();
            if (overlay && overlay.mode !== null) {
                annot.setOverlayMode(null);
                redrawAnnotFull();
            }
        }
        updateToolbarButtonActive();
    };
    tabSource.addEventListener('click', () => showTab('source'));
    tabTranslated.addEventListener('click', () => showTab('translated'));

    const overlayAtOpen = annot.getOverlay();
    if (overlayAtOpen) {
        const translatedText = overlayAtOpen.lines.map((line) => line.dstText || '').join('\n');
        if (hasText(translatedText)) translatedTa.value = translatedText;
        sourceTa.hidden = initialTab !== 'source';
        translatedTa.hidden = initialTab !== 'translated';
    } else {
        sourceTa.hidden = initialTab !== 'source';
        translatedTa.hidden = initialTab !== 'translated';
    }

    const markTranslatedStale = (stale) => {
        tabTranslated.setAttribute('data-stale', stale ? 'true' : 'false');
        translatedTa.setAttribute('data-stale', stale ? 'true' : 'false');
    };

    sourceTa.addEventListener('input', () => {
        if (translatedTa.value) markTranslatedStale(true);
    });

    // 复制
    copyBtn.addEventListener('click', () => {
        const currentTa = sourceTa.hidden ? translatedTa : sourceTa;
        const selected = currentTa.value.slice(currentTa.selectionStart, currentTa.selectionEnd);
        const value = selected || currentTa.value;
        if (value) {
            copyToClipboard(value)
                .then(() => showTransientHint(selected ? '已复制所选文字' : '已复制全文'))
                .catch((e) => console.error('复制失败', e));
        }
        panel.remove();
        updateToolbarButtonActive();
    });

    // 背景策略切换
    if (bgStrategySelect) {
        const overlayCur = annot.getOverlay();
        if (overlayCur && overlayCur.bgStrategy) {
            bgStrategySelect.value = overlayCur.bgStrategy;
        }
        bgStrategySelect.addEventListener('click', (e) => e.stopPropagation());
        bgStrategySelect.addEventListener('change', () => {
            const cur = annot.getOverlay();
            if (!cur) return;
            annot.setOverlay({
                lines: cur.lines,
                mode: cur.mode,
                bgStrategy: bgStrategySelect.value,
                targetLang: cur.translationTargetLang,
            });
            redrawAnnotFull();
        });
    }

    // 字号微调
    if (fontScaleInput) {
        const overlayCur = annot.getOverlay();
        if (overlayCur) fontScaleInput.value = String(Math.round((overlayCur.fontScale ?? 1.0) * 100));
        fontScaleVal.textContent = fontScaleInput.value + '%';
        fontScaleInput.addEventListener('input', () => {
            const pct = parseInt(fontScaleInput.value, 10);
            fontScaleVal.textContent = pct + '%';
            annot.setOverlayFontScale(pct / 100);
            redrawAnnotFull();
        });
    }

    // 翻译
    let translating = false;
    let loadingAnimTimer = null;
    const doTranslate = async () => {
        if (translating) return;
        const src = sourceTa.value.trim();
        if (!src || src === '（未识别到文字）') return;
        translating = true;
        translateBtn.disabled = true;
        translateBtn.textContent = '翻译中…';
        translatedTa.setAttribute('data-loading', 'true');
        translatedTa.value = '翻译中,请稍候…';
        annot.setOverlayLoading(true);
        loadingAnimTimer = setInterval(() => {
            if (!annot.isOverlayLoading()) {
                clearInterval(loadingAnimTimer);
                loadingAnimTimer = null;
                return;
            }
            // H2 优化：用快照+spinner 替代全量重绘
            annot.redrawLoadingSpinner();
        }, 50);
        const overlayLang = annot.getOverlay()?.translationTargetLang;
        requestOverlayTranslation(overlayLang);
        const startTime = Date.now();
        const waitForTranslation = () => {
            const latest = annot.getOverlay();
            const allTranslated = latest && latest.lines.length > 0
                && latest.lines.every((line) => hasText(line.dstText));
            if (allTranslated) {
                translating = false;
                translateBtn.disabled = false;
                translateBtn.textContent = '重新翻译';
                translatedTa.removeAttribute('data-loading');
                if (loadingAnimTimer) {
                    clearInterval(loadingAnimTimer);
                    loadingAnimTimer = null;
                }
                annot.setOverlayLoading(false);
                translatedTa.value = latest.lines.map((line) => line.dstText || '').join('\n');
                markTranslatedStale(false);
                return;
            }
            if (ss.translationBusy) {
                const done = latest ? latest.lines.filter((l) => hasText(l.dstText)).length : 0;
                const total = latest ? latest.lines.length : 0;
                if (total > 0 && done > 0) {
                    translatedTa.value = `翻译中 ${done}/${total}…`;
                }
                if (Date.now() - startTime < 30000) {
                    setTimeout(waitForTranslation, 100);
                    return;
                }
            }
            if (loadingAnimTimer) {
                clearInterval(loadingAnimTimer);
                loadingAnimTimer = null;
            }
            annot.setOverlayLoading(false);
            translating = false;
            translateBtn.disabled = false;
            translateBtn.textContent = '重新翻译';
            translatedTa.removeAttribute('data-loading');
            if (!ss.translationBusy && !(latest && latest.lines.some((l) => hasText(l.dstText)))) {
                translatedTa.value = '翻译失败，请重试';
            }
        };
        setTimeout(waitForTranslation, 100);
    };
    translateBtn.addEventListener('click', () => {
        showTab('translated');
        const advSection = panel.querySelector('.ocr-panel-adv');
        if (advSection) advSection.classList.remove('hidden');
        const overlay = annot.getOverlay();
        if (overlay && overlay.mode !== 'translated') {
            annot.setOverlayMode('translated');
        }
        doTranslate();
    });

    // reading 生命周期与 overlay 绑定
    if (result && Array.isArray(result.words) && result.words.length > 0 && !ss.reading) {
        enterReadingMode(result);
    }
}
