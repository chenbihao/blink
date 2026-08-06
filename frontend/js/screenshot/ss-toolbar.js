//! 截图 overlay 工具栏 + 水印表单 + 文本输入（0.14.6 §4 拆分）。
//!
//! 包含：
//! - bindToolbar()：工具栏事件绑定（下拉菜单/工具切换/颜色/粗细/撤销重做/拖动）
//! - selectTool()：工具切换统一入口
//! - openWatermarkForm()：水印表单视图切换 + 事件绑定
//! - showTextInput()：文本标注输入框
//! - updateUndoRedoButtons()：撤销/重做按钮状态

import { ss, TOOL_CAPS } from './ss-state.js';
import { findDisplayCssAt } from './ss-display.js';
import { drawFinalSelection, redrawAnnotFull } from './ss-draw.js';
import { updateSelectionCursor } from './ss-interaction.js';
import { doCancel, doPinSelection, doSaveSelection, doCopySelection } from './ss-output.js';
// 0.15.7：长截图
import { enterScrollCapture } from './scroll/index.js';
import { doOcrSelection, doTranslateSelection, doTranslateAndPin } from './ss-ocr.js';
import { doOcrDiagnostics } from './ss-ocr-diagnostics.js';
import * as annot from './annotation-engine.js';
import { initColorPicker, syncFromAnnot } from './ss-color-picker.js';

export function updateUndoRedoButtons() {
  const btnUndo = document.getElementById('btn-undo');
  const btnRedo = document.getElementById('btn-redo');
  if (btnUndo) btnUndo.disabled = !annot.canUndo();
  if (btnRedo) btnRedo.disabled = !annot.canRedo();
}

/** 工具切换统一入口 */
function selectTool(tool) {
  const { canvas, hitCanvas } = ss;
  // 0.15.14：聚光灯/多次聚光灯切换时清理旧命令
  // 单次↔多次切换时，旧工具的聚光灯命令不应残留
  if ((tool === 'spotlight' || tool === 'spotlight-multi') && tool !== annot.getTool()) {
    annot.clearSpotlights();
  }
  annot.setTool(tool);
  canvas.setAttribute('data-tool', tool);
  if (ss._longImageBaseCanvas) {
    // 长图底图由独立 canvas 保存；普通选区重绘会把它覆盖回初始全屏截图。
    canvas.style.cursor = tool === 'select' ? 'grab' : 'crosshair';
  } else {
    updateSelectionCursor(-1, -1);
    if (ss.selCss) drawFinalSelection();
  }
  hitCanvas.setAttribute('data-tool', tool);
  document.querySelectorAll('.split-main, .tool-direct').forEach((b) => b.classList.remove('active'));
  document.querySelectorAll('.dropdown-item[data-tool]').forEach((b) => b.classList.remove('active'));
  const group = TOOL_GROUPS[tool];
  const meta = group && group !== 'direct' ? GROUP_META[group] : null;
  if (meta) {
    const item = document.querySelector(`${meta.dropdown} .dropdown-item[data-tool="${tool}"]`);
    if (item) {
      item.classList.add('active');
      const icon = item.querySelector('.item-icon');
      const iconEl = meta.iconId ? document.getElementById(meta.iconId) : null;
      if (iconEl && icon) iconEl.innerHTML = icon.innerHTML;
      const mainEl = meta.mainId ? document.getElementById(meta.mainId) : null;
      if (mainEl) mainEl.dataset.tool = tool;
    }
    const mainEl = meta.mainId ? document.getElementById(meta.mainId) : null;
    if (mainEl) mainEl.classList.add('active');
  } else if (group === 'direct') {
    const btn = document.querySelector(`.tool-direct[data-tool="${tool}"]`);
    if (btn) btn.classList.add('active');
  }
  closeAllDropdowns();

  // 0.15.8-fix：hover 模式下，如果鼠标仍在 dropdown-wrap 上，重新展开
  // 这样点击 dropdown item 后面板不会关闭，方便快速切换
  const hoveredWrap = document.querySelector('.dropdown-wrap:hover');
  if (hoveredWrap) {
    const dd = hoveredWrap.querySelector('.dropdown');
    if (dd) {
      positionDropdown(dd);
      dd.setAttribute('data-open', 'true');
    }
  }

  // 0.15.1→fix：同步模式切换器 active 状态（仅更新当前工具所属组的 dropdown）
  const caps = TOOL_CAPS[tool];
  if (caps && caps.supportMode && caps.modeGroup) {
    const mode = annot.getToolMode(tool);
    // 通过 modeGroup 匹配对应 dropdown：blur→blur-dropdown, eraser→eraser-dropdown
    const dropdownMap = { blur: '#blur-dropdown', eraser: '#eraser-dropdown', highlight: '#stroke-dropdown' };
    const dropdownSel = dropdownMap[caps.modeGroup];
    if (dropdownSel) {
      const switcher = document.querySelector(`${dropdownSel} .mode-switcher`);
      if (switcher) {
        switcher.querySelectorAll('.mode-btn').forEach((b) => {
          b.classList.toggle('active', b.dataset.mode === mode);
        });
      }
    }
  }

  // 0.15.13：pixelate/blur 改为 widthCat='brush'，画笔粗细自动显示
  // effect 类不再需要特殊处理（已统一为 brush）
  const strokeWidthTrigger = document.getElementById('stroke-width-trigger');
  const brushSizeTrigger = document.getElementById('brush-size-trigger');
  const widthCat = caps ? caps.widthCat : null;
  const showBrush = widthCat === 'brush';
  if (strokeWidthTrigger) strokeWidthTrigger.style.display = (widthCat === 'stroke') ? '' : 'none';
  if (brushSizeTrigger) brushSizeTrigger.style.display = showBrush ? '' : 'none';

  if (tool === 'watermark') {
    openWatermarkForm();
  } else if (tool === 'text' || tool === 'number') {
    // 0.15.11：文字/数字工具显示文字配置面板
    showSubPanel('text-config');
  } else {
    // 0.15.11：其他工具隐藏二级面板
    showSubPanel(null);
  }

  // 0.15.9：放大镜子菜单——仅在选中放大镜时显示倍率选项
  const zoomSep = document.querySelector('.magnifier-zoom-sep');
  const zoomRow = document.querySelector('.magnifier-zoom-row');
  if (zoomSep && zoomRow) {
    const show = tool === 'magnifier';
    zoomSep.hidden = !show;
    zoomRow.hidden = !show;
  }
}

const TOOL_GROUPS = {
  select: 'direct',
  rect: 'shape', ellipse: 'shape',
  spotlight: 'shape', 'spotlight-multi': 'shape', magnifier: 'shape',
  pencil: 'stroke', arrow: 'stroke',
  'highlight-multiply': 'stroke', 'highlight-translucent': 'stroke',
  text: 'text', watermark: 'text',
  number: 'text',
  pixelate: 'blur', blur: 'blur',
  // 0.15.12：mosaic 合并到 pixelate，保留映射以兼容旧命令数据
  eraser: 'eraser',
};

// 0.15.0：GROUP_META 从 `let = null` 延迟初始化改为模块级常量，消除 footgun。
// selectTool / bindToolbar 通过 mainId/iconId 按 id 查找 DOM，不在模块加载时强求 DOM 就绪。
const GROUP_META = {
  shape:  { mainId: 'shape-main',  iconId: 'shape-main-icon',  dropdown: '#shape-dropdown' },
  stroke: { mainId: 'stroke-main', iconId: 'stroke-main-icon', dropdown: '#stroke-dropdown' },
  text:   { mainId: 'text-main',   iconId: 'text-main-icon',   dropdown: '#text-dropdown' },
  blur:   { mainId: 'blur-main',   iconId: 'blur-main-icon',   dropdown: '#blur-dropdown' },
  eraser: { mainId: 'eraser-main', iconId: null,               dropdown: '#eraser-dropdown' },
};

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

/** 0.15.12：二级面板显示/隐藏——根据当前工具决定显示哪个视图 */
function showSubPanel(view) {
  const subPanel = document.getElementById('sub-panel');
  if (!subPanel) return;
  if (!view) {
    subPanel.classList.add('hidden');
    return;
  }
  subPanel.classList.remove('hidden');
  subPanel.setAttribute('data-view', view);
  // 0.15.12：定位到对应工具组按钮下方（而非整个工具栏左边缘）
  // 文字组 → text-main 按钮，其他 → 工具栏左侧
  const anchorId = view === 'watermark' || view === 'text-config' ? 'text-main' : null;
  const anchor = anchorId ? document.getElementById(anchorId) : ss.toolbar;
  if (anchor) {
    const rect = anchor.getBoundingClientRect();
    const mon = findDisplayCssAt(rect.left, rect.top);
    let left = rect.left;
    let top = rect.bottom + 4;
    // 不超出屏幕
    const pw = subPanel.offsetWidth || 210;
    if (left + pw > mon.x + mon.w - 8) left = mon.x + mon.w - pw - 8;
    if (left < mon.x + 8) left = mon.x + 8;
    if (top + (subPanel.offsetHeight || 120) > mon.y + mon.h - 8) {
      top = rect.top - (subPanel.offsetHeight || 120) - 4;
    }
    subPanel.style.left = left + 'px';
    subPanel.style.top = top + 'px';
  }
  // 0.15.12：关闭按钮绑定
  const closeBtn = subPanel.querySelector('.sub-panel-close');
  if (closeBtn && !closeBtn._bound) {
    closeBtn._bound = true;
    closeBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      subPanel.classList.add('hidden');
    });
  }
}

/** 水印表单视图切换 + 事件绑定（幂等，0.15.11 改用 sub-panel） */
function openWatermarkForm() {
  showSubPanel('watermark');
  const subPanel = document.getElementById('sub-panel');
  if (!subPanel) return;

  const textInput = subPanel.querySelector('.wm-text');
  const layoutSelect = subPanel.querySelector('.wm-layout');
  const opacityRange = subPanel.querySelector('.wm-opacity');
  const opacityVal = subPanel.querySelector('.wm-opacity-val');
  // 0.15.12：密度控件
  const densityRange = subPanel.querySelector('.wm-density');
  const densityVal = subPanel.querySelector('.wm-density-val');
  const clearBtn = subPanel.querySelector('.wm-clear');

  const existing = annot.getWatermark();
  if (existing) {
    if (textInput) textInput.value = existing.text;
    if (layoutSelect) layoutSelect.value = existing.layout;
    if (opacityRange) {
      opacityRange.value = Math.round(existing.opacity * 100);
      if (opacityVal) opacityVal.textContent = `${opacityRange.value}%`;
    }
    // 0.15.12：回填密度
    if (densityRange && existing.density !== undefined) {
      densityRange.value = Math.round(existing.density * 100);
      if (densityVal) densityVal.textContent = `${densityRange.value}%`;
    }
  }
  if (clearBtn) clearBtn.disabled = !existing;
  if (textInput) setTimeout(() => textInput.focus(), 0);

  if (ss.watermarkFormBound) return;
  ss.watermarkFormBound = true;

  if (opacityRange && opacityVal) {
    opacityRange.addEventListener('input', () => {
      opacityVal.textContent = `${opacityRange.value}%`;
    });
    // 0.15.13：透明度滑块支持滚轮（向上=增大）
    opacityRange.addEventListener('wheel', (e) => {
      e.preventDefault();
      e.stopPropagation();
      const cur = parseInt(opacityRange.value, 10);
      let v = cur + (e.deltaY > 0 ? -1 : 1);
      v = Math.max(0, Math.min(100, v));
      opacityRange.value = v;
      opacityVal.textContent = `${v}%`;
    }, { passive: false });
  }
  // 0.15.12：密度滑块实时反馈
  if (densityRange && densityVal) {
    densityRange.addEventListener('input', () => {
      densityVal.textContent = `${densityRange.value}%`;
    });
    // 0.15.13：密度滑块支持滚轮（向上=增大）
    densityRange.addEventListener('wheel', (e) => {
      e.preventDefault();
      e.stopPropagation();
      const cur = parseInt(densityRange.value, 10);
      let v = cur + (e.deltaY > 0 ? -1 : 1);
      v = Math.max(50, Math.min(300, v));
      densityRange.value = v;
      densityVal.textContent = `${v}%`;
    }, { passive: false });
  }

  const applyBtn = subPanel.querySelector('.wm-apply');
  const apply = () => {
    const text = textInput.value.trim();
    if (!text) {
      if (annot.hasWatermark()) {
        annot.clearWatermark();
        if (clearBtn) clearBtn.disabled = true;
        redrawAnnotFull();
        updateUndoRedoButtons();
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
      // 0.15.12：密度 50-300% → 0.5-3.0
      density: parseInt(densityRange ? densityRange.value : '100', 10) / 100,
    });
    redrawAnnotFull();
    updateUndoRedoButtons();
    if (clearBtn) clearBtn.disabled = false;
  };
  if (applyBtn) applyBtn.addEventListener('click', apply);
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
  subPanel.querySelectorAll('.sub-panel-watermark input, .sub-panel-watermark select, .sub-panel-watermark button')
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
  const cssFontPx = annot.getTextConfig().fontSize / dpr;
  input.style.fontSize = cssFontPx + 'px';
  input.style.fontFamily = annot.getTextConfig().fontFamily;
  if (annot.getTextConfig().bold) input.style.fontWeight = 'bold';
  if (annot.getTextConfig().italic) input.style.fontStyle = 'italic';
  if (annot.getTextConfig().shadow) input.style.textShadow = '0 1px 2px rgba(0,0,0,0.5)';
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

  // 0.15.8-fix：dropdown 变量（后续代码复用）
  const textDropdown = document.getElementById('text-dropdown');
  const blurDropdown = document.getElementById('blur-dropdown');

  // 0.15.8-fix：hover 自动展开 dropdown（替代原 caret 点击展开）
  // 鼠标悬浮在 .dropdown-wrap 上时自动展开 dropdown，移开时延迟关闭。
  // 200ms 延迟确保用户能从触发器移到 dropdown 内部而不意外关闭。
  let hoverCloseTimer = null;
  document.querySelectorAll('.dropdown-wrap').forEach((wrap) => {
    const dd = wrap.querySelector('.dropdown');
    if (!dd) return;
    const isColor = dd.id === 'color-dropdown';

    wrap.addEventListener('mouseenter', () => {
      clearTimeout(hoverCloseTimer);
      closeAllDropdowns();
      positionDropdown(dd);
      dd.setAttribute('data-open', 'true');
      if (isColor) syncFromAnnot();
      // 0.15.14：text dropdown 不再有 data-view，无需重置
    });

    wrap.addEventListener('mouseleave', () => {
      // 0.15.14：所有 dropdown 统一 hover-close
      hoverCloseTimer = setTimeout(() => {
        dd.setAttribute('data-open', 'false');
      }, 200);
    });
  });

  document.addEventListener('click', (e) => {
    if (!e.target.closest('.dropdown-wrap')) closeAllDropdowns();
  });
  document.addEventListener('mousedown', (e) => {
    if (e.target.id === 'canvas') closeAllDropdowns();
  });

  // 0.15.0：GROUP_META 已是模块级常量，不再在 bindToolbar 里赋值
  const shapeMain = document.getElementById('shape-main');
  const strokeMain = document.getElementById('stroke-main');
  const textMain = document.getElementById('text-main');
  const blurMain = document.getElementById('blur-main');
  const eraserMain = document.getElementById('eraser-main');

  [shapeMain, strokeMain, textMain, blurMain, eraserMain].forEach((btn) => {
    if (!btn) return;
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      // 0.15.8-fix：hover 模式下不主动关闭 dropdown，让 mouseleave 自然关闭
      selectTool(btn.dataset.tool);
    });
  });

  // 0.15.0：工具组滚轮轮换——hover split-main 时滚轮循环切换组内工具
  document.querySelectorAll('.dropdown-wrap .split-main').forEach((mainBtn) => {
    mainBtn.addEventListener('wheel', (e) => {
      e.preventDefault();
      e.stopPropagation();
      const groupEntry = Object.entries(GROUP_META).find(([, meta]) => meta.mainId === mainBtn.id);
      if (!groupEntry) return;
      const meta = groupEntry[1];
      const items = document.querySelectorAll(`${meta.dropdown} .dropdown-item[data-tool]`);
      if (items.length < 2) return;
      const currentTool = mainBtn.dataset.tool;
      let idx = Array.from(items).findIndex((it) => it.dataset.tool === currentTool);
      if (idx < 0) idx = 0;
      idx += e.deltaY > 0 ? 1 : -1;  // 0.15.14：恢复正常方向（下滚=下一个）
      idx = Math.max(0, Math.min(items.length - 1, idx));  // 边界 clamp 不循环
      selectTool(items[idx].dataset.tool);
    }, { passive: false });
  });

  document.querySelectorAll('#shape-dropdown .dropdown-item, #stroke-dropdown .dropdown-item, #text-dropdown .dropdown-item, #blur-dropdown .dropdown-item').forEach((item) => {
    item.addEventListener('click', () => selectTool(item.dataset.tool));
  });
  // 0.15.9：放大镜倍率子菜单按钮
  document.querySelectorAll('.zoom-btn').forEach((btn) => {
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      const zoom = parseFloat(btn.dataset.zoom);
      annot.setMagnifierZoom(zoom);
      document.querySelectorAll('.zoom-btn').forEach((b) => b.classList.remove('active'));
      btn.classList.add('active');
      if (ss.selCss && !ss._longImageBaseCanvas) drawFinalSelection();
    });
    btn.addEventListener('mousedown', (e) => e.stopPropagation());
  });
  document.querySelectorAll('.tool-direct').forEach((btn) => {
    if (btn.disabled) return;
    btn.addEventListener('click', () => selectTool(btn.dataset.tool));
  });

  // 0.15.1→fix：模式切换器绑定——点击模式按钮时先切换到对应组的工具再设模式
  document.querySelectorAll('.mode-switcher .mode-btn').forEach((btn) => {
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      const mode = btn.dataset.mode;
      // 找到该 mode-switcher 所属的 dropdown 及其组
      const dropdown = btn.closest('.dropdown');
      if (!dropdown) return;
      const groupEntry = Object.entries(GROUP_META).find(([, meta]) => meta.dropdown === '#' + dropdown.id);
      if (!groupEntry) return;
      const [, meta] = groupEntry;
      const mainBtn = document.getElementById(meta.mainId);
      if (!mainBtn) return;
      // 获取该组当前激活的工具（从 main 按钮的 data-tool）
      let groupTool = mainBtn.dataset.tool;
      if (!groupTool) {
        // 回退：取 dropdown 里第一个带 data-tool 的 item
        const firstItem = dropdown.querySelector('.dropdown-item[data-tool]');
        groupTool = firstItem ? firstItem.dataset.tool : null;
      }
      if (!groupTool) return;
      // 先设模式（这样 selectTool 内部同步模式切换器时能读到新值）
      annot.setToolMode(groupTool, mode);
      // 如果当前工具不在该组，切换到该组的工具
      if (annot.getTool() !== groupTool) {
        selectTool(groupTool);
      } else {
        // 已在该组工具上，只需更新模式切换器 UI
        const switcher = btn.closest('.mode-switcher');
        if (switcher) {
          switcher.querySelectorAll('.mode-btn').forEach((b) => b.classList.remove('active'));
          btn.classList.add('active');
        }
        if (ss.selCss && !ss._longImageBaseCanvas) drawFinalSelection();
      }
      // 0.15.8-fix：hover 模式下重新展开（模式切换后面板保持打开）
      const hoveredWrap = document.querySelector('.dropdown-wrap:hover');
      if (hoveredWrap) {
        const dd = hoveredWrap.querySelector('.dropdown');
        if (dd) {
          positionDropdown(dd);
          dd.setAttribute('data-open', 'true');
        }
      }
    });
  });

  // 0.15.14：模式切换器支持鼠标滚轮切换（画笔↔框选）
  document.querySelectorAll('.mode-switcher').forEach((switcher) => {
    switcher.addEventListener('wheel', (e) => {
      e.preventDefault();
      e.stopPropagation();
      const btns = switcher.querySelectorAll('.mode-btn');
      if (btns.length < 2) return;
      // 找当前激活的模式
      let curIdx = Array.from(btns).findIndex((b) => b.classList.contains('active'));
      if (curIdx < 0) curIdx = 0;
      // 下滚 = 下一个模式，上滚 = 上一个；边界 clamp 不循环
      const newIdx = Math.max(0, Math.min(btns.length - 1, curIdx + (e.deltaY > 0 ? 1 : -1)));
      if (newIdx !== curIdx) btns[newIdx].click();
    }, { passive: false });
  });
  // selectTool 只在用户切换工具时调，首次进入时不会被触发，需在此同步初始状态。
  {
    const initCaps = TOOL_CAPS['select'];
    const initWidthCat = initCaps ? initCaps.widthCat : null;
    const initShowBrush = initWidthCat === 'brush';
    const swt = document.getElementById('stroke-width-trigger');
    const bst = document.getElementById('brush-size-trigger');
    if (swt) swt.style.display = (initWidthCat === 'stroke') ? '' : 'none';
    if (bst) bst.style.display = initShowBrush ? '' : 'none';
  }

  // 0.15.11：强度滑块统一控制三种效果（blur/pixelate/mosaic）
  const blurIntensitySlider = blurDropdown ? blurDropdown.querySelector('.blur-intensity') : null;
  const blurIntensityVal = blurDropdown ? blurDropdown.querySelector('.blur-intensity-val') : null;
  if (blurIntensitySlider) blurIntensitySlider.addEventListener('input', (e) => {
    e.stopPropagation();
    const v = parseInt(e.target.value, 10);
    annot.setEffectConfig({ blurIntensity: v });
    if (blurIntensityVal) blurIntensityVal.textContent = String(v);
  });
  if (blurIntensitySlider) blurIntensitySlider.addEventListener('wheel', (e) => {
    e.preventDefault();
    e.stopPropagation();
    const cur = annot.getEffectConfig().blurIntensity;
    let v = cur + (e.deltaY > 0 ? -1 : 1);  // 0.15.13：向上滚=增大
    v = Math.max(1, Math.min(30, v));
    annot.setEffectConfig({ blurIntensity: v });
    blurIntensitySlider.value = v;
    if (blurIntensityVal) blurIntensityVal.textContent = String(v);
  }, { passive: false });
  // 阻止滑块交互穿透导致下拉关闭
  if (blurDropdown) blurDropdown.querySelectorAll('.blur-intensity')
    .forEach((el) => el.addEventListener('mousedown', (e) => e.stopPropagation()));

  // 0.15.8-fix：eraser dropdown 由 hover 自动展开，无需 trigger click

  // 0.15.11：文字配置控件已移至 sub-panel，不再从 textDropdown 查找
  const subPanel = document.getElementById('sub-panel');
  // 字号滑块
  const fontSizeSlider = subPanel ? subPanel.querySelector('.text-font-size') : null;
  const fontSizeVal = subPanel ? subPanel.querySelector('.text-font-size-val') : null;
  if (fontSizeSlider) fontSizeSlider.addEventListener('input', (e) => {
    e.stopPropagation();
    const size = parseInt(e.target.value, 10);
    annot.setTextConfig({ fontSize: size });
    if (fontSizeVal) fontSizeVal.textContent = String(size);
  });
  // 0.15.13：字号滑块支持鼠标滚轮调值（向上=增大）
  if (fontSizeSlider) fontSizeSlider.addEventListener('wheel', (e) => {
    e.preventDefault();
    e.stopPropagation();
    const cur = parseInt(fontSizeSlider.value, 10);
    let v = cur + (e.deltaY > 0 ? -1 : 1);
    v = Math.max(5, Math.min(72, v));
    fontSizeSlider.value = v;
    annot.setTextConfig({ fontSize: v });
    if (fontSizeVal) fontSizeVal.textContent = String(v);
  }, { passive: false });
  // 字体下拉——改为可搜索的系统字体列表
  const fontPicker = subPanel ? subPanel.querySelector('#font-picker') : null;
  const fontSearch = subPanel ? subPanel.querySelector('.font-search') : null;
  const fontDropdown = subPanel ? subPanel.querySelector('#font-dropdown') : null;
  let systemFonts = [];
  let currentFontFamily = annot.getTextConfig().fontFamily;

  if (fontSearch) {
    // 初始 placeholder 显示当前字体
    fontSearch.value = currentFontFamily;

    // 异步加载系统字体列表
    import('../shared/api.js')
      .then(({ listSystemFonts }) => listSystemFonts())
      .then((fonts) => {
        systemFonts = fonts || [];
        // 确保通用 fallback 在列表中
        if (!systemFonts.includes('sans-serif')) systemFonts.unshift('sans-serif');
        if (!systemFonts.includes('serif')) systemFonts.unshift('serif');
        if (!systemFonts.includes('monospace')) systemFonts.unshift('monospace');
      })
      .catch((e) => console.warn('[screenshot] 加载系统字体列表失败', e));

    // 输入时模糊匹配
    fontSearch.addEventListener('input', () => {
      const q = fontSearch.value.trim().toLowerCase();
      if (!fontDropdown || systemFonts.length === 0) return;
      const matches = q
        ? systemFonts.filter((f) => f.toLowerCase().includes(q)).slice(0, 100)
        : systemFonts.slice(0, 100);
      renderFontDropdown(matches);
      fontDropdown.classList.remove('hidden');
    });

    // 聚焦时展开
    fontSearch.addEventListener('focus', () => {
      if (!fontDropdown || systemFonts.length === 0) return;
      const q = fontSearch.value.trim().toLowerCase();
      const matches = q
        ? systemFonts.filter((f) => f.toLowerCase().includes(q)).slice(0, 100)
        : systemFonts.slice(0, 100);
      renderFontDropdown(matches);
      fontDropdown.classList.remove('hidden');
    });

    // 失焦时延迟关闭（允许点击选项）
    fontSearch.addEventListener('blur', () => {
      setTimeout(() => { if (fontDropdown) fontDropdown.classList.add('hidden'); }, 200);
    });

    // 键盘：Enter 选第一个匹配项
    fontSearch.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && fontDropdown && !fontDropdown.classList.contains('hidden')) {
        const first = fontDropdown.querySelector('.font-item');
        if (first) {
          first.click();
          e.preventDefault();
        }
      }
    });
  }

  function renderFontDropdown(fonts) {
    if (!fontDropdown) return;
    fontDropdown.innerHTML = '';
    for (const font of fonts) {
      const item = document.createElement('button');
      item.className = 'font-item' + (font === currentFontFamily ? ' active' : '');
      item.textContent = font;
      item.style.fontFamily = `"${font}", sans-serif`;
      item.addEventListener('mousedown', (e) => {
        e.preventDefault();
        e.stopPropagation();
        currentFontFamily = font;
        if (fontSearch) fontSearch.value = font;
        annot.setTextConfig({ fontFamily: font });
        fontDropdown.querySelectorAll('.font-item').forEach((b) => b.classList.remove('active'));
        item.classList.add('active');
        fontDropdown.classList.add('hidden');
      });
      fontDropdown.appendChild(item);
    }
  }

  // 阻止字体选择器的事件穿透
  if (fontPicker) fontPicker.querySelectorAll('input, button').forEach((el) =>
    el.addEventListener('mousedown', (e) => e.stopPropagation()));
  // 粗/斜/阴影 toggle
  const boldBtn = subPanel ? subPanel.querySelector('.text-bold-btn') : null;
  const italicBtn = subPanel ? subPanel.querySelector('.text-italic-btn') : null;
  const shadowBtn = subPanel ? subPanel.querySelector('.text-shadow-btn') : null;
  if (boldBtn) boldBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    const tc = annot.getTextConfig();
    annot.setTextConfig({ bold: !tc.bold });
    boldBtn.classList.toggle('active', !tc.bold);
  });
  if (italicBtn) italicBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    const tc = annot.getTextConfig();
    annot.setTextConfig({ italic: !tc.italic });
    italicBtn.classList.toggle('active', !tc.italic);
  });
  if (shadowBtn) shadowBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    const tc = annot.getTextConfig();
    annot.setTextConfig({ shadow: !tc.shadow });
    shadowBtn.classList.toggle('active', !tc.shadow);
  });
  // 0.15.11：阻止 sub-panel 内控件的事件穿透
  if (subPanel) subPanel.querySelectorAll('.sub-panel-view input, .sub-panel-view select, .sub-panel-view button')
    .forEach((el) => el.addEventListener('mousedown', (e) => e.stopPropagation()));

  // 0.15.4：颜色选择——预设色快选 + 完整色盘模块
  initColorPicker();
  const swatches = document.querySelectorAll('.color-swatch');
  const colorTriggerDot = document.getElementById('color-trigger-dot');
  swatches.forEach((btn) => {
    btn.addEventListener('click', () => {
      const color = btn.dataset.color;
      annot.setColor(color);
      swatches.forEach((b) => b.classList.remove('active'));
      btn.classList.add('active');
      if (colorTriggerDot) colorTriggerDot.style.background = color;
      // 0.15.8-fix：hover 模式下不关闭，让 mouseleave 自然关闭
    });
  });

  // 笔画粗细选择（0.15.0：原 width-dropdown 拆为 stroke-width-dropdown + brush-size-dropdown）
  const strokeWidthTrigger = document.getElementById('stroke-width-trigger');
  const strokeWidthDropdown = document.getElementById('stroke-width-dropdown');
  // 0.15.8-fix：由 hover 自动展开
  const strokeWidthTriggerIcon = document.getElementById('stroke-width-trigger-icon');
  const strokeWidthItems = strokeWidthDropdown ? strokeWidthDropdown.querySelectorAll('[data-stroke-width]') : [];
  const strokeSizes = [1, 2, 4, 6, 8];
  strokeWidthItems.forEach((item) => {
    item.addEventListener('click', () => {
      const width = parseInt(item.dataset.strokeWidth, 10);
      annot.setStrokeWidth(width);
      strokeWidthItems.forEach((b) => b.classList.remove('active'));
      item.classList.add('active');
      const icon = item.querySelector('.item-icon');
      if (strokeWidthTriggerIcon && icon) strokeWidthTriggerIcon.innerHTML = icon.innerHTML;
      // 0.15.8-fix：hover 模式下不关闭，让 mouseleave 自然关闭
    });
  });
  // 笔画样式（实线/虚线）
  const strokeStyleItems = strokeWidthDropdown ? strokeWidthDropdown.querySelectorAll('[data-stroke-style]') : [];
  strokeStyleItems.forEach((item) => {
    item.addEventListener('click', () => {
      const style = item.dataset.strokeStyle;
      annot.setStrokeStyle(style);
      strokeStyleItems.forEach((b) => b.classList.remove('active'));
      item.classList.add('active');
      // 0.15.8-fix：更新触发器图标以反映虚线/实线状态
      if (strokeWidthTriggerIcon) {
        const icon = item.querySelector('.item-icon');
        if (icon) strokeWidthTriggerIcon.innerHTML = icon.innerHTML;
      }
      // 0.15.8-fix：hover 模式下不关闭，让 mouseleave 自然关闭
    });
  });

  // 画笔粗细选择
  const brushSizeTrigger = document.getElementById('brush-size-trigger');
  const brushSizeDropdown = document.getElementById('brush-size-dropdown');
  // 0.15.8-fix：由 hover 自动展开
  const brushSizeTriggerIcon = document.getElementById('brush-size-trigger-icon');
  const brushSizeItems = brushSizeDropdown ? brushSizeDropdown.querySelectorAll('[data-brush-size]') : [];
  const brushSizes = [8, 16, 24, 32, 48];
  brushSizeItems.forEach((item) => {
    item.addEventListener('click', () => {
      const size = parseInt(item.dataset.brushSize, 10);
      annot.setBrushSize(size);
      brushSizeItems.forEach((b) => b.classList.remove('active'));
      item.classList.add('active');
      const icon = item.querySelector('.item-icon');
      if (brushSizeTriggerIcon && icon) brushSizeTriggerIcon.innerHTML = icon.innerHTML;
      // 0.15.8-fix：hover 模式下不关闭，让 mouseleave 自然关闭
    });
  });
  // 0.15.1 T5：滚轮优先——悬浮笔画/画笔粗触发器时滚轮调值
  if (strokeWidthTrigger) strokeWidthTrigger.addEventListener('wheel', (e) => {
    e.preventDefault();
    e.stopPropagation();
    const cur = annot.getStrokeWidth();
    let idx = strokeSizes.indexOf(cur);
    if (idx < 0) idx = 2;
    idx += e.deltaY > 0 ? 1 : -1;  // 0.15.14：恢复正常方向（下滚=下一个/更大）
    idx = Math.max(0, Math.min(strokeSizes.length - 1, idx));
    const nw = strokeSizes[idx];
    annot.setStrokeWidth(nw);
    strokeWidthItems.forEach((b) => b.classList.toggle('active', parseInt(b.dataset.strokeWidth, 10) === nw));
    const active = strokeWidthDropdown.querySelector(`[data-stroke-width="${nw}"]`);
    if (active && strokeWidthTriggerIcon) {
      const icon = active.querySelector('.item-icon');
      if (icon) strokeWidthTriggerIcon.innerHTML = icon.innerHTML;
    }
  }, { passive: false });
  if (brushSizeTrigger) brushSizeTrigger.addEventListener('wheel', (e) => {
    e.preventDefault();
    e.stopPropagation();
    const cur = annot.getBrushSize();
    let idx = brushSizes.indexOf(cur);
    if (idx < 0) idx = 1;
    idx += e.deltaY > 0 ? 1 : -1;  // 0.15.14：恢复正常方向（下滚=下一个/更大）
    idx = Math.max(0, Math.min(brushSizes.length - 1, idx));
    const ns = brushSizes[idx];
    annot.setBrushSize(ns);
    brushSizeItems.forEach((b) => b.classList.toggle('active', parseInt(b.dataset.brushSize, 10) === ns));
    const active = brushSizeDropdown.querySelector(`[data-brush-size="${ns}"]`);
    if (active && brushSizeTriggerIcon) {
      const icon = active.querySelector('.item-icon');
      if (icon) brushSizeTriggerIcon.innerHTML = icon.innerHTML;
    }
  }, { passive: false });

  // 撤销/重做
  const btnUndo = document.getElementById('btn-undo');
  const btnRedo = document.getElementById('btn-redo');
  if (btnUndo) btnUndo.addEventListener('click', () => { annot.undo(); updateUndoRedoButtons(); });
  if (btnRedo) btnRedo.addEventListener('click', () => { annot.redo(); updateUndoRedoButtons(); });
  // 0.15.9：重置全部标注
  const btnReset = document.getElementById('btn-reset');
  if (btnReset) btnReset.addEventListener('click', () => {
    annot.clearAll();
    updateUndoRedoButtons();
    redrawAnnotFull();
  });

  // 输出/取消
  const bind = (id, fn) => {
    const el = document.getElementById(id);
    if (el) el.addEventListener('click', fn);
  };
  bind('btn-cancel', doCancel);
  // 0.15.7：长截图入口——只在选区已定时可用（disabled 属性在 index.js 控制）
  bind('btn-scroll', () => {
    if (ss.selCss && !ss.sent) {
      enterScrollCapture(ss.selCss);
    }
  });
  bind('btn-pin', doPinSelection);
  bind('btn-ocr', doOcrSelection);
  bind('btn-ocr-diag', doOcrDiagnostics);
  bind('btn-translate', doTranslateSelection);
  bind('btn-translate-pin', doTranslateAndPin);
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
      // 0.15.12：拖动工具栏时同步移动 sub-panel（相对 text-main 按钮位置）
      const subP = document.getElementById('sub-panel');
      if (subP && !subP.classList.contains('hidden')) {
        const textMain = document.getElementById('text-main');
        if (textMain) {
          const tmRect = textMain.getBoundingClientRect();
          subP.style.left = tmRect.left + 'px';
          subP.style.top = (tmRect.bottom + 4) + 'px';
        } else {
          subP.style.left = left + 'px';
          subP.style.top = (top + th + 4) + 'px';
        }
      }
    });
    document.addEventListener('mouseup', () => {
      if (dragging) {
        dragging = false;
        document.body.style.cursor = '';
      }
    });
  }
}
