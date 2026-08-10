//! 截图 overlay 输出动作（0.14.6 §4 拆分）。
//!
//! 包含：复制选区/全屏、钉图、保存、合成 PNG、取消截图。

import { ss } from './ss-state.js';
import * as annot from './annotation-engine.js';
import { cssRectToBitmap, cssPointToScreen } from './ss-selection-geometry.js';
import {
  screenshotCopy, screenshotCopyRegion, screenshotPin, screenshotSave,
  screenshotCancel, hideScreenshotOverlay,
  imageEditorCopy, imageEditorPin, imageEditorSave, imageEditorCancel,
} from '../shared/api.js';
import { IMAGE_SOURCE } from './image-editor-session.js';
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
  if (!annot.hasAnnotations() && ss.editorSession.canUseCaptureCropFastPath) {
    const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
    // 统一用 cssRectToBitmap 生成 bitmap rect，避免 X/Y scale 分歧
    const bmp = cssRectToBitmap(ss.selCss, meta);
    screenshotCopyRegion(bmp.x, bmp.y, bmp.w, bmp.h)
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
    outputEditorPng('copy', pngBytes)
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
  // 统一用 cssPointToScreen 计算屏幕物理坐标
  const screenPos = ss.editorSession.canvasBacked
    ? { x: ss.editorSession.screenX || ss.scrollBandX, y: ss.editorSession.screenY || ss.scrollBandY }
    : cssPointToScreen(ss.selCss.x, ss.selCss.y, meta);
  const screenX = screenPos.x;
  const screenY = screenPos.y;
  compositeSelection((pngBytes) => {
    outputEditorPng('pin', pngBytes, screenX, screenY)
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

/** 按编辑来源分派输出：截图保留原位 pin/来源标记，普通图片走用户输出适配器。 */
export async function outputEditorPng(action, pngBytes, screenX = 0, screenY = 0, showTranslating = false) {
  if (ss.editorSession.source !== IMAGE_SOURCE.CLIPBOARD) {
    return outputScreenshotPng(action, pngBytes, screenX, screenY, showTranslating);
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
  try {
    const sessionBase = ss.editorSession.baseCanvas;
    const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
    // 统一用 cssRectToBitmap 生成 bitmap rect，快路径与合成路径输出一致
    const bmp = sessionBase ? { x: 0, y: 0, w: sessionBase.width, h: sessionBase.height } : cssRectToBitmap(ss.selCss, meta);
    const px = bmp.x;
    const py = bmp.y;
    const pw = bmp.w;
    const ph = bmp.h;

    const off = document.createElement('canvas');
    off.width = pw;
    off.height = ph;
    const offCtx = off.getContext('2d');
    if (sessionBase) {
      offCtx.drawImage(sessionBase, 0, 0);
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
  if (ss.editorSession.source !== IMAGE_SOURCE.LONG_SCREENSHOT) return;
  if (!ss.scrollSession.active && !ss._imagePan) return;
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
  const source = ss.editorSession.source;
  try { hideSelLoading(); } catch (e) { console.warn('[screenshot] hideSelLoading failed', e); }
  // 0.15.7：长截图状态清理
  if (source === IMAGE_SOURCE.LONG_SCREENSHOT && (ss.scrollSession.active || ss._imagePan)) {
    // 异步清理，不阻塞 cancel；入口由长截图 session 唯一提供。
    Promise.resolve(ss.scrollSession.exit(false)).catch(() => {});
  }
  if (source === IMAGE_SOURCE.CLIPBOARD) {
    imageEditorCancel().catch((e) => console.error('[image-editor] cancel 失败', e));
  } else if (ss.isAnnotating) {
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
