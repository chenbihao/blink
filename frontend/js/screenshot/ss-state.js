//! 截图 overlay 共享状态（0.14.6 §4 拆分）。
//!
//! chord-screenshot.js 拆分为多个子模块后，所有共享状态统一存在此对象中。
//! 各子模块通过 `import { ss } from './ss-state.js'` 访问和修改状态。

import { attachScrollSessionFacade } from './scroll/session.js';
import { ImageEditorSession } from './image-editor-session.js';

export const ss = {
  // ── DOM 引用（initDOM() 填充）─────────────────────────────
  canvas: null,
  ctx: null,
  annotCanvas: null,
  annotCtx: null,
  toolbar: null,
  sizeHint: null,
  errorHint: null,
  strokeCursor: null,
  hitCanvas: null,
  hitCtx: null,

  // ── 选区状态 ──────────────────────────────────────────────
  screenshot: null,          // 全屏截图 Image
  screenshotOffscreen: null, // 0.15.8-fix：纯截图离屏 canvas（无遮罩），供放大镜/取色器读取原始像素
  startX: 0, startY: 0,     // 拖拽起点（CSS 像素）
  endX: 0, endY: 0,         // 拖拽终点（CSS 像素）
  isDragging: false,         // 是否正在选区拖拽
  isAnnotDragging: false,    // 是否正在标注绘制拖拽
  selCss: null,              // 选区 CSS 像素 {x, y, w, h}
  isAnnotating: false,       // 是否在标注模式（选区已确定）
  sent: false,               // 防止复制/保存/钉图重复提交
  singleClickTimeout: null,  // 单击→200ms 后隐藏的定时器
  blurGuard: false,          // blur 事件短窗口防抖

  // ── 标注绘制状态（物理像素）────────────────────────────────
  annotStartX: 0, annotStartY: 0,
  annotCurrentX: 0, annotCurrentY: 0,

  // ── OCR / 翻译状态 ────────────────────────────────────────
  ocrBusy: false,            // 显式 OCR 请求门禁
  translationBusy: false,    // 图上译文请求中
  ocrPrewarm: null,          // Promise<OcrResult> | null
  screenshotConfig: { prewarmOcr: true, scrollDebug: false, ocrDebug: false, controlSnap: false, controlSnapDepth: 15, controlSnapDeadlineMs: 1000, controlSnapMinSize: 50 },
  selectionRevision: 0,
  translationRevision: 0,
  windowListGen: 0,
  // 0.18.2：控件提示列表 generation（与 windowListGen 独立，控件列表异步加载）
  controlHintsGen: 0,
  ocrResultCache: null,      // OCR 结果缓存

  // ── 选区交互状态 ──────────────────────────────────────────
  selectionInteraction: null, // move/resize/new
  cancelInProgress: false,
  // 0.15.8 R2：pending-snap 状态——mousedown 在候选窗口上时不立即吸附，
  // 等到 mouseup 且总位移 < 3px 才采用窗口矩形；达到阈值则转 free-selecting。
  pendingSnap: null, // null | { startX, startY, winRect, pointerId }
  // 0.15.8 R2：吸附窗口的 HWND，供长截图优先使用
  snappedHwnd: null,

  // ── OCR 阅读模式状态 ──────────────────────────────────────
  reading: null,             // 阅读模式数据 { words, lines, charRanges }
  hitEventsBound: false,

  // ── 水印表单状态 ──────────────────────────────────────────
  watermarkFormBound: false,

  // ── 0.15.8：像素放大镜状态 ──────────────────────
  magnifierEl: null,           // #pixel-magnifier DOM
  magnifierCanvas: null,      // .pm-grid canvas
  magnifierCtx: null,          // .pm-grid ctx
  magnifierCoord: null,        // .pm-coord span
  magnifierColor: null,        // .pm-color-text span
  magnifierColorSwatch: null,  // .pm-color-swatch (色块预览)
  magnifierRaf: 0,             // rAF ID
  magnifierFormat: 0,          // 0=HEX, 1=RGB, 2=HSL（Shift 切换）

  // ── 0.15.7：长截图 DOM（会话数据由 ScrollCaptureSession 唯一持有）──
  scrollPreviewCanvas: null,    // 预览缩略图 canvas
  scrollPreviewCtx: null,       // 预览缩略图 ctx

  // ── 加载代际守卫（BUG1 fix）──────────────────────────────
  _loadGen: 0,                  // 每次调用 loadScreenshot 递增；onload 校验防过期回调

  // ── 0.15.9：标注预览 rAF 节流 ──────────────────────────
  _annotRaf: 0,                 // requestAnimationFrame ID（0 = 无待执行帧）

  // ── 0.15.10：已提交命令快照（避免预览时全量重绘）──────────
  // startDraw 时拍快照，redrawAnnotPreview 直接 drawImage 恢复，
  // 不再每帧调 redrawAnnotFull() 重放全部命令。
  _committedSnapshot: null,     // HTMLCanvasElement | null

  // ── 0.15.10：取色器活跃标志（eyedropper 模式时显示像素放大镜）──
  eyedropperActive: false,

  // ── 跨模块回调（主文件注册，避免循环依赖）──────────────────
  _invalidateSelectionContent: null,  // 选区内容失效（清 OCR/阅读/overlay）
  _enterAnnotationMode: null,         // 进入标注模式
  _exitAnnotationMode: null,          // 退出标注模式
  _redrawAnnotPreview: null,          // 标注预览重绘
  _showOcrResult: null,               // 显示 OCR 面板
  _showTransientHint: null,           // 临时提示
  _doCancel: null,                    // 取消截图
  _compositeSelection: null,          // 合成选区 PNG
  _doPinSelection: null,              // 0.18.1：钉图（回调避免 ss-ocr → ss-output 循环依赖）
  _outputEditorPng: null,             // 图片来源感知输出（避免 ss-ocr → ss-output 循环依赖）
  _enterCanvasImageEditor: null,       // 来源无关的 ImageData 编辑入口
  _translateAndPinPending: false,     // 0.18.1：翻译并 pin 流程进行中（防重复点击）
  editorSession: new ImageEditorSession(), // 截图/长图/剪贴板共用的图片编辑会话
};

attachScrollSessionFacade(ss);

// ── 常量 ──────────────────────────────────────────────────────
export const SELECTION_HANDLE_SIZE = 8;
export const MIN_SELECTION_SIZE = 5;
export const PREWARM_MIN_WIDTH = 100;
export const PREWARM_MIN_HEIGHT = 50;

// ── 0.15.1：TOOL_CAPS 工具能力表 ──────────────────────────────
//
// 每个工具的能力描述，消除此前 brush-family 列表重复 5 次的问题。
// 所有 switch/if 改读此表。
//
// 字段说明：
// - points:       'box'（start+end 两点）/ 'stream'（完整点序列）/ null（非绘制工具）
// - widthCat:     决定读 config 哪一层（stroke/brush/text/effect/null）
// - widthMul:     旧的魔法乘数（高亮×4 等迁入 config 层后此字段可废弃，过渡期保留）
// - hasCursor:    是否有笔画预览虚圈
// - minDrag:      最小拖拽阈值（0 = 单击也生效，3 = 需明显拖拽）
// - supportMode:  是否支持框选/画笔模式切换
// modeGroup: supportMode=true 的工具按组共享模式（画笔/框选）。
// 同组工具切换时模式保持一致，不会「同组切换就变回去」。
// 'blur' 组：mosaic/pixelate/blur；'eraser' 组：eraser；'highlight' 组：两种荧光笔。
export const TOOL_CAPS = {
  select:                   { points: null,     widthCat: null,    widthMul: 0, hasCursor: false, minDrag: 0, supportMode: false, modeGroup: null },
  rect:                     { points: 'box',   widthCat: 'stroke', widthMul: 1, hasCursor: false, minDrag: 3, supportMode: false, modeGroup: null },
  ellipse:                  { points: 'box',   widthCat: 'stroke', widthMul: 1, hasCursor: false, minDrag: 3, supportMode: false, modeGroup: null },
  arrow:                    { points: 'box',   widthCat: 'stroke', widthMul: 1, hasCursor: false, minDrag: 3, supportMode: false, modeGroup: null },
  pencil:                   { points: 'stream',widthCat: 'stroke', widthMul: 1, hasCursor: true,  minDrag: 0, supportMode: false, modeGroup: null },
  'highlight-multiply':     { points: 'stream', widthCat: 'brush',  widthMul: 1, hasCursor: true,  minDrag: 0, supportMode: true,  modeGroup: 'highlight' },
  'highlight-translucent':  { points: 'stream', widthCat: 'brush',  widthMul: 1, hasCursor: true,  minDrag: 0, supportMode: true,  modeGroup: 'highlight' },
  mosaic:                    { points: 'stream',widthCat: 'brush',  widthMul: 1, hasCursor: true,  minDrag: 0, supportMode: true,  modeGroup: 'blur' },
  pixelate:                  { points: 'stream',widthCat: 'brush',  widthMul: 1, hasCursor: true,  minDrag: 0, supportMode: true,  modeGroup: 'blur' },  // 画笔=轨迹遮罩马赛克，框选=矩形马赛克
  blur:                      { points: 'stream',widthCat: 'brush',  widthMul: 1, hasCursor: true,  minDrag: 0, supportMode: true,  modeGroup: 'blur' },  // 0.15.13：blur 也支持画笔模式 + 画笔粗细
  eraser:                    { points: 'stream',widthCat: 'brush',  widthMul: 1, hasCursor: true,  minDrag: 0, supportMode: true,  modeGroup: 'eraser' },
  text:                      { points: 'box',   widthCat: 'text',   widthMul: 0, hasCursor: false, minDrag: 0, supportMode: false, modeGroup: null },
  number:                    { points: 'box',   widthCat: 'brush',  widthMul: 0, hasCursor: true,  minDrag: 0, supportMode: false, modeGroup: null },
  spotlight:                 { points: 'box',   widthCat: null,     widthMul: 0, hasCursor: false, minDrag: 3, supportMode: false, modeGroup: null },
  'spotlight-multi':         { points: 'box',   widthCat: null,     widthMul: 0, hasCursor: false, minDrag: 3, supportMode: false, modeGroup: null },
  magnifier:                 { points: 'box',   widthCat: null,     widthMul: 0, hasCursor: false, minDrag: 3, supportMode: false, modeGroup: null },
};

/** 初始化 DOM 引用（在模块加载后、使用前调用一次） */
export function initDOM() {
  ss.canvas = document.getElementById('canvas');
  ss.ctx = ss.canvas.getContext('2d', { willReadFrequently: true });
  ss.annotCanvas = document.getElementById('annot-canvas');
  ss.annotCtx = ss.annotCanvas.getContext('2d');
  ss.toolbar = document.getElementById('toolbar');
  ss.sizeHint = document.getElementById('size-hint');
  ss.errorHint = document.getElementById('error-hint');
  ss.strokeCursor = document.getElementById('stroke-cursor');
  ss.magnifierEl = document.getElementById('pixel-magnifier');
  if (ss.magnifierEl) {
    ss.magnifierCanvas = ss.magnifierEl.querySelector('.pm-grid');
    ss.magnifierCtx = ss.magnifierCanvas ? ss.magnifierCanvas.getContext('2d') : null;
    ss.magnifierCoord = ss.magnifierEl.querySelector('.pm-coord');
    ss.magnifierColor = ss.magnifierEl.querySelector('.pm-color-text');
    ss.magnifierColorSwatch = ss.magnifierEl.querySelector('.pm-color-swatch');
  }
  ss.hitCanvas = document.getElementById('ocr-hit-canvas');
  ss.hitCtx = ss.hitCanvas ? ss.hitCanvas.getContext('2d') : null;
  // 0.15.7：长截图预览缩略图
  ss.scrollPreviewCanvas = document.getElementById('scroll-preview');
  ss.scrollPreviewCtx = ss.scrollPreviewCanvas ? ss.scrollPreviewCanvas.getContext('2d') : null;
}
