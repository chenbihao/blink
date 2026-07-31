//! 截图 overlay 工具栏 + 水印表单 + 文本输入（0.14.6 §4 拆分）。
//!
//! 包含：
//! - bindToolbar()：工具栏事件绑定（下拉菜单/工具切换/颜色/粗细/撤销重做/拖动）
//! - selectTool()：工具切换统一入口
//! - openWatermarkForm()：水印表单视图切换 + 事件绑定
//! - showTextInput()：文本标注输入框
//! - updateUndoRedoButtons()：撤销/重做按钮状态

import { ss } from './ss-state.js';
import { findDisplayCssAt } from './ss-display.js';
import { drawFinalSelection, redrawAnnotFull } from './ss-draw.js';
import { updateSelectionCursor } from './ss-interaction.js';
import { doCancel, doPinSelection, doSaveSelection, doCopySelection } from './ss-output.js';
import { doOcrSelection, doTranslateSelection } from './ss-ocr.js';
import * as annot from './annotation-engine.js';

export function updateUndoRedoButtons() {
  const btnUndo = document.getElementById('btn-undo');
  const btnRedo = document.getElementById('btn-redo');
  if (btnUndo) btnUndo.disabled = !annot.canUndo();
  if (btnRedo) btnRedo.disabled = !annot.canRedo();
}

/** 工具切换统一入口 */
function selectTool(tool) {
  const { canvas, hitCanvas } = ss;
  annot.setTool(tool);
  canvas.setAttribute('data-tool', tool);
  updateSelectionCursor(-1, -1);
  if (ss.selCss) drawFinalSelection();
  hitCanvas.setAttribute('data-tool', tool);
  document.querySelectorAll('.split-main, .tool-direct').forEach((b) => b.classList.remove('active'));
  document.querySelectorAll('.dropdown-item[data-tool]').forEach((b) => b.classList.remove('active'));
  const group = TOOL_GROUPS[tool];
  const meta = GROUP_META[group];
  if (meta) {
    const item = document.querySelector(`${meta.dropdown} .dropdown-item[data-tool="${tool}"]`);
    if (item) {
      item.classList.add('active');
      const icon = item.querySelector('.item-icon');
      if (meta.icon && icon) meta.icon.innerHTML = icon.innerHTML;
      if (meta.main) meta.main.dataset.tool = tool;
    }
    if (meta.main) meta.main.classList.add('active');
  } else if (group === 'direct') {
    const btn = document.querySelector(`.tool-direct[data-tool="${tool}"]`);
    if (btn) btn.classList.add('active');
  }
  closeAllDropdowns();

  if (tool === 'watermark') {
    openWatermarkForm();
  }
}

const TOOL_GROUPS = {
  select: 'direct',
  rect: 'shape', ellipse: 'shape',
  arrow: 'stroke', pencil: 'stroke',
  'highlight-multiply': 'stroke', 'highlight-translucent': 'stroke',
  text: 'text', watermark: 'text',
  pixelate: 'blur', mosaic: 'blur',
  eraser: 'direct',
};

let GROUP_META = null;

function closeAllDropdowns() {
  document.querySelectorAll('.dropdown').forEach((d) => d.setAttribute('data-open', 'false'));
}

function positionDropdown(dropdown) {
  if (!dropdown) return;
  dropdown.removeAttribute('data-placement');
  const wrap = dropdown.closest('.dropdown-wrap');
  const anchor = wrap ? wrap.getBoundingClientRect() : ss.toolbar.getBoundingClientRect();
  const mon = findDisplayCssAt(anchor.left, anchor.top);
  dropdown.style.visibility = 'hidden';
  dropdown.setAttribute('data-open', 'true');
  const dh = dropdown.offsetHeight;
  if (anchor.bottom + 4 + dh > mon.y + mon.h - 8 && anchor.top - 4 - dh >= mon.y + 8) {
    dropdown.setAttribute('data-placement', 'top');
  }
  dropdown.style.visibility = '';
}

function toggleDropdown(dropdown) {
  const willOpen = dropdown.getAttribute('data-open') !== 'true';
  closeAllDropdowns();
  if (willOpen) positionDropdown(dropdown);
}

/** 水印表单视图切换 + 事件绑定（幂等） */
function openWatermarkForm() {
  const dropdown = document.getElementById('text-dropdown');
  if (!dropdown) return;

  dropdown.setAttribute('data-view', 'watermark');
  dropdown.setAttribute('data-open', 'true');

  const textInput = dropdown.querySelector('.wm-text');
  const layoutSelect = dropdown.querySelector('.wm-layout');
  const opacityRange = dropdown.querySelector('.wm-opacity');
  const opacityVal = dropdown.querySelector('.wm-opacity-val');
  const clearBtn = dropdown.querySelector('.wm-clear');

  const existing = annot.getWatermark();
  if (existing) {
    if (textInput) textInput.value = existing.text;
    if (layoutSelect) layoutSelect.value = existing.layout;
    if (opacityRange) {
      opacityRange.value = Math.round(existing.opacity * 100);
      if (opacityVal) opacityVal.textContent = `${opacityRange.value}%`;
    }
  }
  if (clearBtn) clearBtn.disabled = !existing;
  if (textInput) setTimeout(() => textInput.focus(), 0);

  if (ss.watermarkFormBound) return;
  ss.watermarkFormBound = true;

  const backToList = () => {
    dropdown.setAttribute('data-view', 'list');
  };

  if (opacityRange && opacityVal) {
    opacityRange.addEventListener('input', () => {
      opacityVal.textContent = `${opacityRange.value}%`;
    });
  }

  const applyBtn = dropdown.querySelector('.wm-apply');
  const backBtn = dropdown.querySelector('.wm-back');
  const apply = () => {
    const text = textInput.value.trim();
    if (!text) {
      if (annot.hasWatermark()) {
        annot.clearWatermark();
        if (clearBtn) clearBtn.disabled = true;
        redrawAnnotFull();
        updateUndoRedoButtons();
        backToList();
        dropdown.setAttribute('data-open', 'false');
      } else {
        textInput.focus();
      }
      return;
    }
    annot.commitWatermark({
      text,
      layout: layoutSelect.value,
      color: annot.getColor(),
      width: annot.getWidth(),
      opacity: parseInt(opacityRange.value, 10) / 100,
    });
    redrawAnnotFull();
    updateUndoRedoButtons();
    if (clearBtn) clearBtn.disabled = false;
    backToList();
    dropdown.setAttribute('data-open', 'false');
  };
  if (applyBtn) applyBtn.addEventListener('click', apply);
  if (backBtn) backBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    backToList();
  });
  if (clearBtn) clearBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    annot.clearWatermark();
    if (textInput) textInput.value = '';
    clearBtn.disabled = true;
    redrawAnnotFull();
    updateUndoRedoButtons();
  });
  if (textInput) textInput.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); apply(); }
  });
  dropdown.querySelectorAll('.dropdown-view-watermark input, .dropdown-view-watermark select, .dropdown-view-watermark button')
    .forEach((el) => el.addEventListener('mousedown', (e) => e.stopPropagation()));
}

/** 文本标注输入框 */
export function showTextInput(x, y) {
  if (!ss.selCss) return;
  const dpr = window.devicePixelRatio || 1;

  const input = document.createElement('span');
  input.className = 'text-annot-input';
  input.contentEditable = 'true';
  input.setAttribute('role', 'textbox');
  input.setAttribute('data-placeholder', '输入文本…');
  input.style.left = (ss.selCss.x + x / dpr) + 'px';
  input.style.top = (ss.selCss.y + y / dpr) + 'px';
  const cssFontPx = (annot.getWidth() * 6) / dpr;
  input.style.fontSize = cssFontPx + 'px';
  input.style.fontFamily = 'sans-serif';
  input.style.lineHeight = '1';
  input.style.color = annot.getColor();
  input.spellcheck = false;
  document.body.appendChild(input);
  input.focus();

  setTimeout(() => {
    const range = document.createRange();
    range.selectNodeContents(input);
    const sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
  }, 0);

  const getText = () => (input.textContent || '').trim();

  let cleanedUp = false;
  const cleanup = () => {
    if (cleanedUp) return;
    cleanedUp = true;
    if (input.parentNode) input.parentNode.removeChild(input);
  };
  const commit = (text) => {
    if (text) { annot.commitText(text); redrawAnnotFull(); updateUndoRedoButtons(); }
    else { annot.cancelText(); }
    cleanup();
  };

  input.addEventListener('keydown', (e) => {
    if (e.isComposing || e.keyCode === 229) return;
    if (e.key === 'Enter') {
      e.preventDefault();
      commit(getText());
    } else if (e.key === 'Escape') {
      e.stopPropagation();
      annot.cancelText();
      cleanup();
    }
  });

  input.addEventListener('blur', () => {
    commit(getText());
  });
}

/** 工具栏事件绑定（模块内直接绑定，替代 HTML 内联脚本） */
export function bindToolbar() {
  const { toolbar } = ss;

  const shapeTrigger = document.getElementById('shape-trigger');
  const strokeTrigger = document.getElementById('stroke-trigger');
  const textTrigger = document.getElementById('text-trigger');
  const blurTrigger = document.getElementById('blur-trigger');
  const colorTrigger = document.getElementById('color-trigger');
  const widthTrigger = document.getElementById('width-trigger');
  const shapeDropdown = document.getElementById('shape-dropdown');
  const strokeDropdown = document.getElementById('stroke-dropdown');
  const textDropdown = document.getElementById('text-dropdown');
  const blurDropdown = document.getElementById('blur-dropdown');
  const colorDropdown = document.getElementById('color-dropdown');
  const widthDropdown = document.getElementById('width-dropdown');

  if (shapeTrigger) shapeTrigger.addEventListener('click', (e) => { e.stopPropagation(); toggleDropdown(shapeDropdown); });
  if (strokeTrigger) strokeTrigger.addEventListener('click', (e) => { e.stopPropagation(); toggleDropdown(strokeDropdown); });
  if (textTrigger) textTrigger.addEventListener('click', (e) => { e.stopPropagation(); toggleDropdown(textDropdown); });
  if (blurTrigger) blurTrigger.addEventListener('click', (e) => { e.stopPropagation(); toggleDropdown(blurDropdown); });
  if (colorTrigger) colorTrigger.addEventListener('click', (e) => { e.stopPropagation(); toggleDropdown(colorDropdown); });
  if (widthTrigger) widthTrigger.addEventListener('click', (e) => { e.stopPropagation(); toggleDropdown(widthDropdown); });

  document.addEventListener('click', (e) => {
    if (!e.target.closest('.dropdown-wrap')) closeAllDropdowns();
  });
  document.addEventListener('mousedown', (e) => {
    if (e.target.id === 'canvas') closeAllDropdowns();
  });

  const shapeMain = document.getElementById('shape-main');
  const shapeMainIcon = document.getElementById('shape-main-icon');
  const strokeMain = document.getElementById('stroke-main');
  const strokeMainIcon = document.getElementById('stroke-main-icon');
  const textMain = document.getElementById('text-main');
  const textMainIcon = document.getElementById('text-main-icon');
  const blurMain = document.getElementById('blur-main');
  const blurMainIcon = document.getElementById('blur-main-icon');

  GROUP_META = {
    shape:  { main: shapeMain,  icon: shapeMainIcon,  dropdown: '#shape-dropdown' },
    stroke: { main: strokeMain, icon: strokeMainIcon, dropdown: '#stroke-dropdown' },
    text:   { main: textMain,   icon: textMainIcon,   dropdown: '#text-dropdown' },
    blur:   { main: blurMain,   icon: blurMainIcon,   dropdown: '#blur-dropdown' },
  };

  [shapeMain, strokeMain, textMain, blurMain].forEach((btn) => {
    if (!btn) return;
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      closeAllDropdowns();
      selectTool(btn.dataset.tool);
    });
  });

  document.querySelectorAll('#shape-dropdown .dropdown-item, #stroke-dropdown .dropdown-item, #text-dropdown .dropdown-item, #blur-dropdown .dropdown-item').forEach((item) => {
    item.addEventListener('click', () => selectTool(item.dataset.tool));
  });
  document.querySelectorAll('.tool-direct').forEach((btn) => {
    btn.addEventListener('click', () => selectTool(btn.dataset.tool));
  });

  // 颜色选择
  const swatches = document.querySelectorAll('.color-swatch');
  const colorPicker = document.getElementById('color-picker');
  const colorTriggerDot = document.getElementById('color-trigger-dot');
  swatches.forEach((btn) => {
    btn.addEventListener('click', () => {
      const color = btn.dataset.color;
      annot.setColor(color);
      swatches.forEach((b) => b.classList.remove('active'));
      btn.classList.add('active');
      if (colorPicker) colorPicker.value = color;
      if (colorTriggerDot) colorTriggerDot.style.background = color;
      closeAllDropdowns();
    });
  });
  if (colorPicker) {
    colorPicker.addEventListener('input', (e) => {
      const color = e.target.value;
      annot.setColor(color);
      swatches.forEach((b) => b.classList.remove('active'));
      if (colorTriggerDot) colorTriggerDot.style.background = color;
    });
  }

  // 粗细选择
  const widthTriggerIcon = document.getElementById('width-trigger-icon');
  const widthItems = widthDropdown ? widthDropdown.querySelectorAll('.dropdown-item') : [];
  widthItems.forEach((item) => {
    item.addEventListener('click', () => {
      const width = parseInt(item.dataset.width, 10);
      annot.setWidth(width);
      widthItems.forEach((b) => b.classList.remove('active'));
      item.classList.add('active');
      const icon = item.querySelector('.item-icon');
      if (widthTriggerIcon && icon) widthTriggerIcon.innerHTML = icon.innerHTML;
      closeAllDropdowns();
    });
  });

  // 撤销/重做
  const btnUndo = document.getElementById('btn-undo');
  const btnRedo = document.getElementById('btn-redo');
  if (btnUndo) btnUndo.addEventListener('click', () => { annot.undo(); updateUndoRedoButtons(); });
  if (btnRedo) btnRedo.addEventListener('click', () => { annot.redo(); updateUndoRedoButtons(); });

  // 输出/取消
  const bind = (id, fn) => {
    const el = document.getElementById(id);
    if (el) el.addEventListener('click', fn);
  };
  bind('btn-cancel', doCancel);
  bind('btn-pin', doPinSelection);
  bind('btn-ocr', doOcrSelection);
  bind('btn-translate', doTranslateSelection);
  bind('btn-save', doSaveSelection);
  bind('btn-copy', doCopySelection);

  // 拖动 handle
  const dragHandle = document.getElementById('toolbar-drag');
  if (dragHandle) {
    let dragging = false;
    let offsetX = 0, offsetY = 0;
    dragHandle.addEventListener('mousedown', (e) => {
      if (e.button !== 0) return;
      e.preventDefault();
      e.stopPropagation();
      dragging = true;
      toolbar.dataset.userMoved = 'true';
      const rect = toolbar.getBoundingClientRect();
      offsetX = e.clientX - rect.left;
      offsetY = e.clientY - rect.top;
      document.body.style.cursor = 'grabbing';
    });
    document.addEventListener('mousemove', (e) => {
      if (!dragging) return;
      const tw = toolbar.offsetWidth;
      const th = toolbar.offsetHeight;
      const mon = findDisplayCssAt(e.clientX, e.clientY);
      const MARGIN = 8;
      let left = e.clientX - offsetX;
      let top = e.clientY - offsetY;
      left = Math.max(mon.x + MARGIN, Math.min(left, mon.x + mon.w - tw - MARGIN));
      top = Math.max(mon.y + MARGIN, Math.min(top, mon.y + mon.h - th - MARGIN));
      toolbar.style.left = left + 'px';
      toolbar.style.top = top + 'px';
    });
    document.addEventListener('mouseup', () => {
      if (dragging) {
        dragging = false;
        document.body.style.cursor = '';
      }
    });
  }
}
