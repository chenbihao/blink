//! 截图 overlay OCR 阅读模式（0.14.6 §4 拆分）。
//!
//! OCR 完成后 overlay 不关，进入"阅读模式"：
//! - 原图 word 可鼠标拖选（跨行按 word 数组顺序取连续段，不是矩形框选）
//! - 图上拖选 → 面板 textarea 里对应字符高亮 + selection
//! - 面板 textarea 里选中文字 → 反查覆盖的 words → 图上高亮
//! - 用户在 textarea 里手动编辑后，反向映射失效（内容偏移），此后只保留正向
//!
//! 坐标系：word.bounding_rect 是物理像素相对裁剪区左上角（与 annot-canvas 一致）
//!        hit-canvas 内部像素 = 物理像素，CSS 尺寸 = 选区 CSS 尺寸

import { ss } from './ss-state.js';
import { findDisplayCssAt } from './ss-display.js';
import { getSelectionHandle, beginSelectionInteraction, updateSelectionInteraction, finishSelectionInteraction } from './ss-interaction.js';
import { copyToClipboard } from '../shared/api.js';

/** 从 result 构造 charRanges —— 用与后端 join_words_smart 相同的规则复算字符偏移。 */
function computeCharRanges(words) {
  const ranges = new Array(words.length);
  let cursor = 0;
  let prevLine = null;
  let prevTailKind = null;
  for (let i = 0; i < words.length; i++) {
    const w = words[i];
    if (!w.text) {
      ranges[i] = { start: cursor, end: cursor };
      continue;
    }
    if (prevLine !== null && prevLine !== w.lineIndex) {
      cursor += 1; // '\n'
      prevTailKind = null;
    }
    if (prevTailKind !== null) {
      const hk = charKind(w.text.charAt(0));
      const needSpace = !(prevTailKind === 'cjk' || hk === 'cjk');
      if (needSpace) cursor += 1;
    }
    const start = cursor;
    cursor += w.text.length; // UTF-16 长度以对齐 textarea selection API
    ranges[i] = { start, end: cursor };
    prevTailKind = charKind(w.text.charAt(w.text.length - 1));
    prevLine = w.lineIndex;
  }
  return ranges;
}

/** 判定字符分类（'cjk' / 'latin' / 'other'）——与后端 is_cjk_ish/is_latin_word_char 对齐 */
function charKind(ch) {
  if (!ch) return 'other';
  const cp = ch.codePointAt(0);
  if (
    (cp >= 0x3400 && cp <= 0x4dbf) ||
    (cp >= 0x4e00 && cp <= 0x9fff) ||
    (cp >= 0x20000 && cp <= 0x2a6df) ||
    (cp >= 0xf900 && cp <= 0xfaff) ||
    (cp >= 0x3040 && cp <= 0x309f) ||
    (cp >= 0x30a0 && cp <= 0x30ff) ||
    (cp >= 0xac00 && cp <= 0xd7af) ||
    (cp >= 0x3000 && cp <= 0x303f) ||
    (cp >= 0xff00 && cp <= 0xffef)
  ) return 'cjk';
  if (/[a-zA-Z0-9]/.test(ch)) return 'latin';
  if ((cp >= 0x00c0 && cp <= 0x024f) || (cp >= 0x1e00 && cp <= 0x1eff)) return 'latin';
  return 'other';
}

/** 命中测试：把点击的 CSS 坐标(相对 hitCanvas)映射到物理像素 + 找命中 word */
function hitTestWord(cssX, cssY) {
  if (!ss.reading) return -1;
  // C 类：hit canvas backing store = 物理像素，bitmap↔CSS = overlay dpr。
  const dpr = window.devicePixelRatio || 1;
  const px = cssX * dpr;
  const py = cssY * dpr;
  for (let i = 0; i < ss.reading.words.length; i++) {
    const r = ss.reading.words[i].rect;
    if (px >= r.x && px <= r.x + r.w && py >= r.y && py <= r.y + r.h) return i;
  }
  return -1;
}

/** 找出接近点击点(垂直方向)的最近 word——空白处点击时靠近哪 word 就选哪 */
function nearestWordByLine(cssX, cssY) {
  if (!ss.reading || ss.reading.words.length === 0) return -1;
  const dpr = window.devicePixelRatio || 1;
  const px = cssX * dpr;
  const py = cssY * dpr;
  let bestLine = ss.reading.words[0].lineIndex;
  let bestDy = Infinity;
  for (const w of ss.reading.words) {
    const cy = w.rect.y + w.rect.h / 2;
    const dy = Math.abs(cy - py);
    if (dy < bestDy) { bestDy = dy; bestLine = w.lineIndex; }
  }
  let bestIdx = -1;
  let bestDx = Infinity;
  for (let i = 0; i < ss.reading.words.length; i++) {
    const w = ss.reading.words[i];
    if (w.lineIndex !== bestLine) continue;
    const cx = w.rect.x + w.rect.w / 2;
    const dx = Math.abs(cx - px);
    if (dx < bestDx) { bestDx = dx; bestIdx = i; }
  }
  return bestIdx;
}

/** 重绘 hit-canvas：高亮当前选中 words + hover word */
function redrawHitLayer() {
  if (!ss.reading) return;
  const { hitCtx, hitCanvas } = ss;
  hitCtx.clearRect(0, 0, hitCanvas.width, hitCanvas.height);
  if (ss.reading.selectionStart !== null && ss.reading.selectionEnd !== null) {
    const lo = Math.min(ss.reading.selectionStart, ss.reading.selectionEnd);
    const hi = Math.max(ss.reading.selectionStart, ss.reading.selectionEnd);
    hitCtx.fillStyle = 'rgba(74, 158, 255, 0.35)';
    for (let i = lo; i <= hi; i++) {
      const r = ss.reading.words[i].rect;
      hitCtx.fillRect(r.x, r.y, r.w, r.h);
    }
    hitCtx.strokeStyle = 'rgba(74, 158, 255, 0.85)';
    hitCtx.lineWidth = Math.max(1, Math.round(window.devicePixelRatio || 1));
    for (let i = lo; i <= hi; i++) {
      const r = ss.reading.words[i].rect;
      hitCtx.strokeRect(r.x + 0.5, r.y + 0.5, r.w, r.h);
    }
  }
  if (ss.reading.hoverWord !== null && ss.reading.hoverWord >= 0) {
    const r = ss.reading.words[ss.reading.hoverWord].rect;
    hitCtx.strokeStyle = 'rgba(255, 255, 255, 0.5)';
    hitCtx.lineWidth = 1;
    hitCtx.strokeRect(r.x + 0.5, r.y + 0.5, r.w, r.h);
  }
}

/** 进入阅读模式：定位 hit-canvas + 装事件 + 首次全选 */
export function enterReadingMode(result) {
  if (!ss.selCss) return;
  const words = (result && Array.isArray(result.words)) ? result.words : [];
  if (words.length === 0) return;

  const dpr = window.devicePixelRatio || 1;
  const { hitCanvas } = ss;
  hitCanvas.style.left = ss.selCss.x + 'px';
  hitCanvas.style.top = ss.selCss.y + 'px';
  hitCanvas.style.width = ss.selCss.w + 'px';
  hitCanvas.style.height = ss.selCss.h + 'px';
  hitCanvas.width = Math.max(1, Math.round(ss.selCss.w * dpr));
  hitCanvas.height = Math.max(1, Math.round(ss.selCss.h * dpr));
  hitCanvas.setAttribute('data-reading', 'true');

  ss.reading = {
    words: words.map((w) => ({
      text: w.text,
      rect: w.rect,
      lineIndex: w.line_index,
    })),
    charRanges: computeCharRanges(words.map((w) => ({
      text: w.text,
      lineIndex: w.line_index,
    }))),
    fullText: result.text || '',
    selectionStart: null,
    selectionEnd: null,
    panelDirty: false,
    dragStart: null,
    hoverWord: null,
  };

  bindHitCanvasEvents();
  redrawHitLayer();
}

/** 退出阅读模式 */
export function exitReadingMode() {
  const { hitCanvas, hitCtx } = ss;
  hitCanvas.removeAttribute('data-reading');
  hitCanvas.removeAttribute('data-resizing');
  hitCtx.clearRect(0, 0, hitCanvas.width, hitCanvas.height);
  ss.reading = null;
}

// ── hit-canvas 事件（幂等绑定，模块生命周期只装一次） ──
function bindHitCanvasEvents() {
  if (ss.hitEventsBound) return;
  ss.hitEventsBound = true;

  const { hitCanvas } = ss;

  const beginPointerSelection = (kind, e, handle = null) => {
    const viewportEvent = { offsetX: e.clientX, offsetY: e.clientY };
    beginSelectionInteraction(kind, viewportEvent, handle);
    if (ss.selectionInteraction && typeof hitCanvas.setPointerCapture === 'function') {
      hitCanvas.setPointerCapture(e.pointerId);
    }
  };

  hitCanvas.addEventListener('pointerdown', (e) => {
    if (!ss.reading || e.button !== 0) return;
    const handle = getSelectionHandle(e.clientX, e.clientY, ss.selCss);
    if (handle) {
      e.stopPropagation();
      e.preventDefault();
      hitCanvas.setAttribute('data-resizing', 'true');
      beginPointerSelection('resize', e, handle);
      return;
    }
    let idx = hitTestWord(e.offsetX, e.offsetY);
    if (idx < 0) idx = nearestWordByLine(e.offsetX, e.offsetY);
    if (idx < 0) return;
    e.stopPropagation();
    e.preventDefault();
    ss.reading.dragStart = idx;
    ss.reading.selectionStart = idx;
    ss.reading.selectionEnd = idx;
    redrawHitLayer();
    syncSelectionToPanel();
    if (typeof hitCanvas.setPointerCapture === 'function') hitCanvas.setPointerCapture(e.pointerId);
  });

  hitCanvas.addEventListener('pointermove', (e) => {
    if (ss.selectionInteraction) {
      updateSelectionInteraction({ offsetX: e.clientX, offsetY: e.clientY });
      return;
    }
    if (!ss.reading) return;
    const idx = hitTestWord(e.offsetX, e.offsetY);
    hitCanvas.style.cursor = 'text';
    if (ss.reading.dragStart !== null) {
      const endIdx = idx >= 0 ? idx : nearestWordByLine(e.offsetX, e.offsetY);
      if (endIdx >= 0) {
        ss.reading.selectionEnd = endIdx;
        redrawHitLayer();
        syncSelectionToPanel();
      }
    } else {
      if (idx !== ss.reading.hoverWord) {
        ss.reading.hoverWord = idx >= 0 ? idx : null;
        redrawHitLayer();
      }
    }
  });

  const finishHitPointer = (e) => {
    if (ss.selectionInteraction) {
      hitCanvas.removeAttribute('data-resizing');
      hitCanvas.style.cursor = 'text';
      finishSelectionInteraction({ offsetX: e.clientX, offsetY: e.clientY });
    } else if (ss.reading) {
      ss.reading.dragStart = null;
    }
    if (typeof hitCanvas.hasPointerCapture === 'function' && hitCanvas.hasPointerCapture(e.pointerId)) {
      hitCanvas.releasePointerCapture(e.pointerId);
    }
  };
  hitCanvas.addEventListener('pointerup', finishHitPointer);
  hitCanvas.addEventListener('pointercancel', finishHitPointer);

  hitCanvas.addEventListener('mouseleave', () => {
    if (!ss.reading || ss.selectionInteraction) return;
    ss.reading.hoverWord = null;
    ss.reading.dragStart = null;
    hitCanvas.style.cursor = 'text';
    redrawHitLayer();
  });

  // 双击选一整行——若面板未开则先召唤面板，之后再高亮整行
  hitCanvas.addEventListener('dblclick', (e) => {
    if (!ss.reading) return;
    let idx = hitTestWord(e.offsetX, e.offsetY);
    if (idx < 0) idx = nearestWordByLine(e.offsetX, e.offsetY);
    if (idx < 0) return;
    const line = ss.reading.words[idx].lineIndex;
    let lo = idx, hi = idx;
    while (lo > 0 && ss.reading.words[lo - 1].lineIndex === line) lo--;
    while (hi < ss.reading.words.length - 1 && ss.reading.words[hi + 1].lineIndex === line) hi++;
    ss.reading.selectionStart = lo;
    ss.reading.selectionEnd = hi;
    redrawHitLayer();
    if (!document.getElementById('ocr-panel') && ss.ocrResultCache) {
      if (typeof ss._showOcrResult === 'function') ss._showOcrResult(ss.ocrResultCache);
    }
    syncSelectionToPanel();
  });

  // 右键菜单（阅读模式激活时弹菜单）
  hitCanvas.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    const selText = getReadingSelectionText();
    showReadingContextMenu(selText || null, e);
  });
}

/** 图上选中变化 → 同步到面板 textarea */
function syncSelectionToPanel() {
  const panel = document.getElementById('ocr-panel');
  if (!panel || !ss.reading) return;
  if (ss.reading.selectionStart === null) return;
  const ta = panel.querySelector('#ocr-textarea-source');
  if (!ta || ta.hidden) return;
  if (ss.reading.panelDirty) return;
  const lo = Math.min(ss.reading.selectionStart, ss.reading.selectionEnd);
  const hi = Math.max(ss.reading.selectionStart, ss.reading.selectionEnd);
  const cs = ss.reading.charRanges[lo].start;
  const ce = ss.reading.charRanges[hi].end;
  ta.focus();
  ta.setSelectionRange(cs, ce);
}

/** 面板 textarea 选中变化 → 反查 words 并高亮图 */
export function syncSelectionFromPanel(ta) {
  if (!ss.reading) return;
  if (ss.reading.panelDirty) return;
  const cs = ta.selectionStart;
  const ce = ta.selectionEnd;
  if (cs === ce) {
    ss.reading.selectionStart = null;
    ss.reading.selectionEnd = null;
    redrawHitLayer();
    return;
  }
  let lo = -1, hi = -1;
  for (let i = 0; i < ss.reading.charRanges.length; i++) {
    const r = ss.reading.charRanges[i];
    if (r.end > cs && lo === -1) lo = i;
    if (r.start < ce) hi = i;
    if (r.start >= ce) break;
  }
  if (lo === -1 || hi === -1 || lo > hi) return;
  ss.reading.selectionStart = lo;
  ss.reading.selectionEnd = hi;
  redrawHitLayer();
}

/** 按当前 word 选择范围拼出文本 */
export function getReadingSelectionText() {
  if (!ss.reading || ss.reading.selectionStart === null || ss.reading.selectionEnd === null) return '';
  const lo = Math.min(ss.reading.selectionStart, ss.reading.selectionEnd);
  const hi = Math.max(ss.reading.selectionStart, ss.reading.selectionEnd);
  const cs = ss.reading.charRanges[lo]?.start;
  const ce = ss.reading.charRanges[hi]?.end;
  if (!Number.isInteger(cs) || !Number.isInteger(ce)) return '';
  return ss.reading.fullText.slice(cs, ce);
}

export function copyReadingSelection() {
  const text = getReadingSelectionText();
  if (!text) return false;
  copyToClipboard(text)
    .then(() => { if (typeof ss._showTransientHint === 'function') ss._showTransientHint('已复制所选文字'); })
    .catch((e) => console.error('[screenshot] 复制识别文字失败', e));
  return true;
}

/** 阅读模式右键菜单：复制（选区或全文）/ 取消截图。跟随鼠标定位。 */
export function showReadingContextMenu(text, mouseEvent) {
  const old = document.getElementById('reading-ctx-menu');
  if (old) old.remove();

  const MARGIN = 8;
  let x = mouseEvent.clientX;
  let y = mouseEvent.clientY;
  const menuW = 140, menuH = 80;
  const mon = findDisplayCssAt(x, y);
  if (x + menuW > mon.x + mon.w - MARGIN) x = mon.x + mon.w - menuW - MARGIN;
  if (y + menuH > mon.y + mon.h - MARGIN) y = mon.y + mon.h - menuH - MARGIN;
  x = Math.max(mon.x + MARGIN, x);
  y = Math.max(mon.y + MARGIN, y);

  const menu = document.createElement('div');
  menu.id = 'reading-ctx-menu';
  menu.style.cssText = `
    position: fixed; left: ${x}px; top: ${y}px; z-index: 9999;
    background: rgba(30,30,30,0.96); border: 1px solid rgba(255,255,255,0.15);
    border-radius: 8px; padding: 4px; min-width: 120px;
    box-shadow: 0 4px 16px rgba(0,0,0,0.5);
    font-family: "Segoe UI","Microsoft YaHei",sans-serif; font-size: 13px;
  `;
  const makeItem = (label, fn) => {
    const btn = document.createElement('div');
    btn.textContent = label;
    btn.style.cssText = `
      padding: 6px 12px; color: #ddd; cursor: pointer; border-radius: 6px;
    `;
    btn.addEventListener('mouseenter', () => btn.style.background = 'rgba(255,255,255,0.08)');
    btn.addEventListener('mouseleave', () => btn.style.background = '');
    btn.addEventListener('click', () => { fn(); menu.remove(); });
    return btn;
  };

  const copyLabel = text ? '复制' : '复制全文';
  const copyText = text || (ss.reading ? ss.reading.fullText : '');
  if (copyText) {
    menu.appendChild(makeItem(copyLabel, () => {
      copyToClipboard(copyText)
        .then(() => { if (typeof ss._showTransientHint === 'function') ss._showTransientHint(text ? '已复制所选文字' : '已复制全文'); })
        .catch((e) => console.error('复制失败', e));
    }));
  }

  // 取消截图——通过回调调用主文件的 doCancel
  menu.appendChild(makeItem('取消截图', () => { if (typeof ss._doCancel === 'function') ss._doCancel(); }));

  document.body.appendChild(menu);

  const close = (ev) => {
    if (!menu.contains(ev.target)) { menu.remove(); document.removeEventListener('pointerdown', close); }
  };
  setTimeout(() => document.addEventListener('pointerdown', close), 0);
}
