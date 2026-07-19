//! 标注引擎（0.11.7-b）：7 种标注工具 + 撤销/重做 + 颜色/粗细。
//!
//! 标注数据模型：
//! ```typescript
//! interface AnnotationCommand {
//!   type: 'rect' | 'ellipse' | 'arrow' | 'pencil' | 'text' | 'mosaic' | 'eraser';
//!   points: {x: number, y: number}[];  // 物理像素坐标，相对裁剪区左上角
//!   color?: string;
//!   width?: number;
//!   fill?: boolean;
//!   text?: string;
//! }
//! ```
//!
//! 标注坐标使用**物理像素**（canvas 内部像素）坐标系，与裁剪区像素对齐。
//! 前端鼠标事件 `offsetX/Y` 为 CSS 像素，需乘 `devicePixelRatio` 转物理像素。

/** 当前工具类型 */
let currentTool = 'rect';
/** 当前颜色 */
let currentColor = '#ff0000';
/** 当前粗细 */
let currentWidth = 4;
/** 是否填充（矩形/椭圆） */
let currentFill = false;
/** 标注历史栈 */
let commands = [];
/** 当前撤销位置（-1 = 无撤销，0 = 已撤销到开头） */
let undoIndex = -1;
/** 当前绘制中的点序列（铅笔/橡皮擦用） */
let currentPoints = [];
/** 绘制起点（矩形/椭圆/箭头/马赛克用） */
let drawStartX = 0, drawStartY = 0;
/** 等待输入文字的临时命令（文本工具用） */
let pendingTextCmd = null;
/** 标注 canvas 上下文 */
let ctx = null;
/** 标注 canvas 元素 */
let canvas = null;
/** 原始裁剪区图像（用于马赛克/橡皮擦恢复） */
let cropImageData = null;

// ── 初始化和重置 ──────────────────────────────────────

/** 绑定标注 canvas */
export function init(annotCanvas) {
  canvas = annotCanvas;
  ctx = annotCanvas.getContext('2d');
}

/** 重置标注状态（新选区时调） */
export function reset(cropW, cropH, cropImageDataRef) {
  commands = [];
  undoIndex = -1;
  cropImageData = cropImageDataRef;
  if (canvas) {
    canvas.width = cropW;
    canvas.height = cropH;
    ctx.clearRect(0, 0, canvas.width, canvas.height);
  }
}

// ── 工具切换 ──────────────────────────────────────────

export function setTool(tool) {
  currentTool = tool;
}

export function getTool() {
  return currentTool;
}

export function setColor(color) {
  currentColor = color;
}

export function getColor() {
  return currentColor;
}

export function setWidth(width) {
  currentWidth = width;
}

export function getWidth() {
  return currentWidth;
}

export function setFill(fill) {
  currentFill = fill;
}

export function getFill() {
  return currentFill;
}

// ── 绘制操作 ──────────────────────────────────────────

/** 开始绘制（工具按下时调） */
export function startDraw(x, y) {
  drawStartX = x;
  drawStartY = y;
  currentPoints = [{ x, y }];
  return currentTool;
}

/** 拖拽绘制中 */
export function moveDraw(x, y) {
  if (currentTool === 'pencil' || currentTool === 'eraser') {
    currentPoints.push({ x, y });
  }
}

/** 获取当前绘制中的点序列（供主脚本实时预览用）。 */
export function getCurrentPoints() {
  return currentPoints;
}

/** 结束绘制，生成 AnnotationCommand */
export function endDraw(x, y) {
  const points = [...currentPoints];
  const lastPoint = { x, y };
  // 对于非铅笔工具，用起点+终点
  let cmdPoints;
  if (currentTool === 'pencil' || currentTool === 'eraser') {
    cmdPoints = points;
  } else {
    cmdPoints = [{ x: drawStartX, y: drawStartY }, lastPoint];
  }

  const cmd = {
    type: currentTool,
    points: cmdPoints,
    color: currentColor,
    width: currentWidth,
    fill: currentFill,
  };

  // 如果是文本工具，需要用户输入文字；通过回调交给主脚本处理
  if (currentTool === 'text') {
    // 保存临时命令，等待文本输入完成
    pendingTextCmd = cmd;
    currentPoints = [];
    return { needsText: true, x: drawStartX, y: drawStartY };
  }

  // 裁剪掉 undoIndex 之后的命令（新命令覆盖重做历史）
  commands = commands.slice(0, undoIndex + 1);
  commands.push(cmd);
  undoIndex = commands.length - 1;

  // 重绘
  redrawAll();

  // 清理
  currentPoints = [];
  return { needsText: false };
}

/** 提交文本标注（由主脚本在用户确认文本后调用） */
export function commitText(text) {
  if (!pendingTextCmd) return;
  pendingTextCmd.text = text;
  commands = commands.slice(0, undoIndex + 1);
  commands.push(pendingTextCmd);
  undoIndex = commands.length - 1;
  pendingTextCmd = null;
  redrawAll();
}

/** 取消文本标注 */
export function cancelText() {
  pendingTextCmd = null;
  currentPoints = [];
}

/** 执行一个标注命令（撤销/重做时重绘用，外部合成时也调） */
export function executeCommand(cmd, targetCtx) {
  const c = targetCtx || ctx;
  if (!c) return;
  c.save();
  // 根据 cmd.type 绘制
  switch (cmd.type) {
    case 'rect':
      if (cmd.points.length >= 2) {
        const [p1, p2] = cmd.points;
        const x = Math.min(p1.x, p2.x);
        const y = Math.min(p1.y, p2.y);
        const w = Math.abs(p2.x - p1.x);
        const h = Math.abs(p2.y - p1.y);
        c.strokeStyle = cmd.color || currentColor;
        c.lineWidth = cmd.width || currentWidth;
        c.strokeRect(x, y, w, h);
        if (cmd.fill) {
          c.fillStyle = cmd.color || currentColor;
          c.globalAlpha = 0.2;
          c.fillRect(x, y, w, h);
        }
      }
      break;
    case 'ellipse':
      if (cmd.points.length >= 2) {
        const [p1, p2] = cmd.points;
        const cx = (p1.x + p2.x) / 2;
        const cy = (p1.y + p2.y) / 2;
        const rx = Math.abs(p2.x - p1.x) / 2;
        const ry = Math.abs(p2.y - p1.y) / 2;
        c.strokeStyle = cmd.color || currentColor;
        c.lineWidth = cmd.width || currentWidth;
        c.beginPath();
        c.ellipse(cx, cy, rx, ry, 0, 0, Math.PI * 2);
        c.stroke();
        if (cmd.fill) {
          c.fillStyle = cmd.color || currentColor;
          c.globalAlpha = 0.2;
          c.fill();
        }
      }
      break;
    case 'arrow':
      if (cmd.points.length >= 2) {
        const [p1, p2] = cmd.points;
        const angle = Math.atan2(p2.y - p1.y, p2.x - p1.x);
        const headLen = 12 * (cmd.width || currentWidth) / 2;
        c.strokeStyle = cmd.color || currentColor;
        c.lineWidth = cmd.width || currentWidth;
        c.beginPath();
        c.moveTo(p1.x, p1.y);
        c.lineTo(p2.x, p2.y);
        c.stroke();
        // 箭头头部
        c.beginPath();
        c.moveTo(p2.x, p2.y);
        c.lineTo(p2.x - headLen * Math.cos(angle - 0.4), p2.y - headLen * Math.sin(angle - 0.4));
        c.moveTo(p2.x, p2.y);
        c.lineTo(p2.x - headLen * Math.cos(angle + 0.4), p2.y - headLen * Math.sin(angle + 0.4));
        c.stroke();
      }
      break;
    case 'pencil':
      if (cmd.points.length >= 2) {
        c.strokeStyle = cmd.color || currentColor;
        c.lineWidth = cmd.width || currentWidth;
        c.lineCap = 'round';
        c.lineJoin = 'round';
        c.beginPath();
        c.moveTo(cmd.points[0].x, cmd.points[0].y);
        for (let i = 1; i < cmd.points.length; i++) {
          c.lineTo(cmd.points[i].x, cmd.points[i].y);
        }
        c.stroke();
      }
      break;
    case 'text':
      if (cmd.text && cmd.points.length >= 1) {
        const p = cmd.points[0];
        c.font = `${(cmd.width || currentWidth) * 6}px sans-serif`;
        c.fillStyle = cmd.color || currentColor;
        c.fillText(cmd.text, p.x, p.y);
      }
      break;
    case 'mosaic':
      // 马赛克：缩小再放大（nearest-neighbor）
      if (cmd.points.length >= 2 && cropImageData) {
        const [p1, p2] = cmd.points;
        const mx = Math.min(p1.x, p2.x);
        const my = Math.min(p1.y, p2.y);
        const mw = Math.abs(p2.x - p1.x);
        const mh = Math.abs(p2.y - p1.y);
        if (mw > 4 && mh > 4) {
          const blockSize = 8;
          const tempCanvas = document.createElement('canvas');
          tempCanvas.width = mw;
          tempCanvas.height = mh;
          const tempCtx = tempCanvas.getContext('2d');
          // 从 cropImageData 中复制原始区域
          tempCtx.putImageData(cropImageData, -mx, -my);
          // 缩小到 1/blockSize
          const smallW = Math.max(1, Math.round(mw / blockSize));
          const smallH = Math.max(1, Math.round(mh / blockSize));
          const smallCanvas = document.createElement('canvas');
          smallCanvas.width = smallW;
          smallCanvas.height = smallH;
          const smallCtx = smallCanvas.getContext('2d');
          smallCtx.imageSmoothingEnabled = false;
          smallCtx.drawImage(tempCanvas, 0, 0, mw, mh, 0, 0, smallW, smallH);
          // 放大回原尺寸
          c.imageSmoothingEnabled = false;
          c.drawImage(smallCanvas, 0, 0, smallW, smallH, mx, my, mw, mh);
          c.imageSmoothingEnabled = true;
        }
      }
      break;
    case 'eraser':
      // 橡皮擦：擦除标注层内容
      if (cmd.points.length >= 2 && c) {
        const [p1, p2] = cmd.points;
        const ex = Math.min(p1.x, p2.x);
        const ey = Math.min(p1.y, p2.y);
        const ew = Math.abs(p2.x - p1.x);
        const eh = Math.abs(p2.y - p1.y);
        c.clearRect(ex, ey, ew, eh);
      }
      break;
  }
  c.restore();
}

// ── 撤销/重做 ──────────────────────────────────────────

export function undo() {
  if (undoIndex < 0) return false;
  undoIndex--;
  redrawAll();
  return true;
}

export function redo() {
  if (undoIndex >= commands.length - 1) return false;
  undoIndex++;
  redrawAll();
  return true;
}

export function canUndo() {
  return undoIndex >= 0;
}

export function canRedo() {
  return undoIndex < commands.length - 1;
}

/** 全量重绘标注层 */
function redrawAll() {
  if (!ctx || !canvas) return;
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  // 重绘到 undoIndex 位置
  for (let i = 0; i <= undoIndex; i++) {
    executeCommand(commands[i]);
  }
}

// ── 输出 ──────────────────────────────────────────────

/** 获取当前标注命令列表（序列化用） */
export function getCommands() {
  return commands.slice(0, undoIndex + 1);
}

/** 是否有标注 */
export function hasAnnotations() {
  return commands.length > 0;
}