//! 截图 overlay 输出动作（0.14.6 §4 拆分）。
//!
//! 包含：复制选区/全屏、钉图、保存、合成 PNG、取消截图。

import { ss } from './ss-state.js';
import * as annot from './annotation-engine.js';
import { cssToBitmap, cssToScreen, cssSizeToPhysical } from './ss-selection-geometry.js';
import {
  screenshotCopy, screenshotCopyRegion, screenshotPin, screenshotSave,
  screenshotCancel, hideScreenshotOverlay,
} from '../shared/api.js';
import { showTransientHint, hideSelLoading } from './ss-ocr.js';

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
  console.info('[screenshot] copy selection', { hasAnnot: annot.hasAnnotations() });

  // 快路径：无标注 → 后端直接从 SESSION 裁剪 BGRA 写剪贴板
  if (!annot.hasAnnotations() && !ss._longImageBaseCanvas) {
    const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
    const bmp = cssToBitmap(ss.selCss.x, ss.selCss.y, meta);
    // B 类：裁剪宽高按选区所在屏 dpr 折算为物理尺寸
    const pw = cssSizeToPhysical(ss.selCss.w, ss.selCss.x, ss.selCss.y, meta);
    const ph = cssSizeToPhysical(ss.selCss.h, ss.selCss.x, ss.selCss.y, meta);
    const px = bmp.x;
    const py = bmp.y;
    screenshotCopyRegion(px, py, pw, ph)
      .then(() => console.info('[screenshot] copy 成功（快路径）'))
      .catch((err) => {
        console.error('[screenshot] copy 失败（快路径）', err);
        ss.errorHint.textContent = '截图保存失败：' + err;
        ss.errorHint.classList.remove('hidden');
        ss.sent = false;
      });
    return;
  }

  compositeSelection((pngBytes) => {
    outputScreenshotPng('copy', pngBytes)
      .then(() => {
        console.info('[screenshot] copy 成功');
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
  console.info('[screenshot] copy fullscreen');
  screenshotCopyRegion(0, 0, ss.canvas.width, ss.canvas.height)
    .then(() => console.info('[screenshot] fullscreen copy 成功（快路径）'))
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
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  // 0.15.8 R0：统一用 cssToScreen 计算屏幕物理坐标（0.18.8 per-monitor）
  const screenPos = ss._longImageBaseCanvas
    ? { x: ss.scrollBandX, y: ss.scrollBandY }
    : cssToScreen(ss.selCss.x, ss.selCss.y, meta);
  const screenX = screenPos.x;
  const screenY = screenPos.y;
  compositeSelection((pngBytes) => {
    outputScreenshotPng('pin', pngBytes, screenX, screenY)
      .then(() => cleanupLongCapture())
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
    outputScreenshotPng('save', pngBytes)
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
 * 同时兼容旧 callback 调用和 Promise 调用；长图编辑时底图来自 `_longImageBaseCanvas`。
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
export { compositeSelectionBytes };

/** 将 ImageData 编码为后端输出接口统一接收的 PNG 字节。 */
export async function encodeImageDataPng(imageData) {
  if (!imageData) return null;
  const canvas = document.createElement('canvas');
  canvas.width = imageData.width;
  canvas.height = imageData.height;
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

async function compositeSelectionBytes() {
  if (!ss.selCss || (!ss.screenshot && !ss._longImageBaseCanvas)) {
    console.error('[screenshot] compositeSelection: no selection or screenshot');
    ss.sent = false;
    return null;
  }
  try {
    const longBase = ss._longImageBaseCanvas;
    const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
    // B 类：裁剪宽高按选区所在屏 dpr 折算为物理尺寸
    const pw = longBase?.width ?? cssSizeToPhysical(ss.selCss.w, ss.selCss.x, ss.selCss.y, meta);
    const ph = longBase?.height ?? cssSizeToPhysical(ss.selCss.h, ss.selCss.x, ss.selCss.y, meta);
    // 0.15.8 R0：统一用 cssToBitmap 计算裁剪区起点（0.18.8 per-monitor）
    const bmp = longBase ? { x: 0, y: 0 } : cssToBitmap(ss.selCss.x, ss.selCss.y, meta);
    const px = bmp.x;
    const py = bmp.y;

    const off = document.createElement('canvas');
    off.width = pw;
    off.height = ph;
    const offCtx = off.getContext('2d');
    if (longBase) {
      offCtx.drawImage(longBase, 0, 0);
    } else {
      offCtx.drawImage(ss.screenshot, px, py, pw, ph, 0, 0, pw, ph);
    }
    if (annot.hasAnnotations()) {
      offCtx.drawImage(ss.annotCanvas, 0, 0);
    }
    const blob = await new Promise((resolve) => off.toBlob(resolve, 'image/png'));
    if (!blob) {
      console.error('[screenshot] PNG 合成失败（blob=null）');
      ss.sent = false;
      return null;
    }
    return new Uint8Array(await blob.arrayBuffer());
  } catch (e) {
    console.error('[screenshot] compositeSelection threw', e);
    ss.sent = false;
    return null;
  }
}

function cleanupLongCapture() {
  if (!ss.scrollSession.active && !ss._longImagePan) return;
  Promise.resolve(ss.scrollSession.exit(false))
    .catch((e) => console.warn('[screenshot] long capture cleanup failed', e));
}

export function doCancel() {
  if (ss.cancelInProgress) {
    console.warn('[screenshot] doCancel: cancelInProgress still true, ignoring');
    return;
  }
  ss.cancelInProgress = true;
  setTimeout(() => { ss.cancelInProgress = false; }, 2000);
  console.info('[screenshot] cancel');
  try { hideSelLoading(); } catch (e) { console.warn('[screenshot] hideSelLoading failed', e); }
  // 0.15.7：长截图状态清理
  if (ss.scrollSession.active || ss._longImagePan) {
    // 异步清理，不阻塞 cancel；入口由长截图 session 唯一提供。
    Promise.resolve(ss.scrollSession.exit(false)).catch(() => {});
  }
  if (ss.isAnnotating) {
    screenshotCancel().catch((e) => console.error('[screenshot] screenshotCancel 失败', e));
  } else {
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
