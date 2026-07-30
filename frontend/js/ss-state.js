//! 截图 overlay 共享状态（0.14.6 §4 拆分）。
//!
//! chord-screenshot.js 拆分为多个子模块后，所有共享状态统一存在此对象中。
//! 各子模块通过 `import { ss } from './ss-state.js'` 访问和修改状态。

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
  screenshotConfig: { prewarmOcr: true },
  selectionRevision: 0,
  translationRevision: 0,
  ocrResultCache: null,      // OCR 结果缓存

  // ── 选区交互状态 ──────────────────────────────────────────
  selectionInteraction: null, // move/resize/new
  cancelInProgress: false,

  // ── OCR 阅读模式状态 ──────────────────────────────────────
  reading: null,             // 阅读模式数据 { words, lines, charRanges }
  hitEventsBound: false,

  // ── 水印表单状态 ──────────────────────────────────────────
  watermarkFormBound: false,

  // ── 跨模块回调（主文件注册，避免循环依赖）──────────────────
  _invalidateSelectionContent: null,  // 选区内容失效（清 OCR/阅读/overlay）
  _enterAnnotationMode: null,         // 进入标注模式
  _exitAnnotationMode: null,          // 退出标注模式
  _redrawAnnotPreview: null,          // 标注预览重绘
  _showOcrResult: null,               // 显示 OCR 面板
  _showTransientHint: null,           // 临时提示
  _doCancel: null,                    // 取消截图
  _compositeSelection: null,          // 合成选区 PNG
};

// ── 常量 ──────────────────────────────────────────────────────
export const SELECTION_HANDLE_SIZE = 8;
export const MIN_SELECTION_SIZE = 5;
export const PREWARM_MIN_WIDTH = 100;
export const PREWARM_MIN_HEIGHT = 50;

/** 初始化 DOM 引用（在模块加载后、使用前调用一次） */
export function initDOM() {
  ss.canvas = document.getElementById('canvas');
  ss.ctx = ss.canvas.getContext('2d');
  ss.annotCanvas = document.getElementById('annot-canvas');
  ss.annotCtx = ss.annotCanvas.getContext('2d');
  ss.toolbar = document.getElementById('toolbar');
  ss.sizeHint = document.getElementById('size-hint');
  ss.errorHint = document.getElementById('error-hint');
  ss.strokeCursor = document.getElementById('stroke-cursor');
  ss.hitCanvas = document.getElementById('ocr-hit-canvas');
  ss.hitCtx = ss.hitCanvas ? ss.hitCanvas.getContext('2d') : null;
}
