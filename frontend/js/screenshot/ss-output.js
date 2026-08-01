//! 截图 overlay 输出动作（0.14.6 §4 拆分）。
//!
//! 包含：复制选区/全屏、钉图、保存、合成 PNG、取消截图。

import { ss } from './ss-state.js';
import * as annot from './annotation-engine.js';
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
  if (!annot.hasAnnotations()) {
    const dpr = window.devicePixelRatio || 1;
    const px = Math.round(ss.selCss.x * dpr);
    const py = Math.round(ss.selCss.y * dpr);
    const pw = Math.round(ss.selCss.w * dpr);
    const ph = Math.round(ss.selCss.h * dpr);
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
    screenshotCopy(pngBytes)
      .then(() => console.info('[screenshot] copy 成功'))
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
  const dpr = window.devicePixelRatio || 1;
  const meta = window.__blinkScreenMeta || { vx: 0, vy: 0 };
  const screenX = Math.round(meta.vx + ss.selCss.x * dpr);
  const screenY = Math.round(meta.vy + ss.selCss.y * dpr);
  compositeSelection((pngBytes) => {
    screenshotPin(pngBytes, screenX, screenY).catch((err) => {
      console.error('[screenshot] pin 失败', err);
      ss.sent = false;
    });
  });
}

export function doSaveSelection() {
  if (!ss.selCss || ss.sent || !ensureOutputReady()) return;
  ss.sent = true;
  compositeSelection((pngBytes) => {
    screenshotSave(pngBytes, null).catch((err) => {
      if (err !== '用户取消了保存') {
        console.error('[screenshot] save 失败', err);
      }
      ss.sent = false;
    });
  });
}

/** 合成选区（裁剪区 + 标注）为 PNG bytes */
export function compositeSelection(callback) {
  if (!ss.selCss || !ss.screenshot) {
    console.error('[screenshot] compositeSelection: no selection or screenshot');
    ss.sent = false;
    return;
  }
  try {
    const dpr = window.devicePixelRatio || 1;
    const pw = Math.round(ss.selCss.w * dpr);
    const ph = Math.round(ss.selCss.h * dpr);
    const px = Math.round(ss.selCss.x * dpr);
    const py = Math.round(ss.selCss.y * dpr);

    const off = document.createElement('canvas');
    off.width = pw;
    off.height = ph;
    const offCtx = off.getContext('2d');
    offCtx.drawImage(ss.screenshot, px, py, pw, ph, 0, 0, pw, ph);
    if (annot.hasAnnotations()) {
      offCtx.drawImage(ss.annotCanvas, 0, 0);
    }
    off.toBlob((blob) => {
      if (!blob) { console.error('[screenshot] PNG 合成失败（blob=null）'); ss.sent = false; return; }
      blob.arrayBuffer().then((buf) => callback(new Uint8Array(buf))).catch((err) => {
        console.error('[screenshot] blob.arrayBuffer 失败', err);
        ss.sent = false;
      });
    }, 'image/png');
  } catch (e) {
    console.error('[screenshot] compositeSelection threw', e);
    ss.sent = false;
  }
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
