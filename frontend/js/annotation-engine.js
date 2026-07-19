//! 标注引擎（0.11.7-b，0.11.8 加 pixelate）：8 种标注工具 + 撤销/重做 + 颜色/粗细。
//!
//! 标注数据模型：
//! ```typescript
//! interface AnnotationCommand {
//!   type: 'rect' | 'ellipse' | 'arrow' | 'pencil' | 'text' | 'mosaic' | 'pixelate' | 'eraser';
//!   points: {x: number, y: number}[];  // 物理像素坐标，相对裁剪区左上角
//!   color?: string;
//!   width?: number;
//!   fill?: boolean;
//!   text?: string;
//! }
//! ```
//!
//! - `mosaic`（涂抹，PixPin 风格）：点序列，圆形笔刷沿轨迹取局部平均色
//! - `pixelate`（经典像素化马赛克）：矩形框选 [起点, 终点]，整个区域分块平均色填充
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
  if (currentTool === 'pencil' || currentTool === 'eraser' || currentTool === 'mosaic') {
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
  // 铅笔/橡皮擦/涂抹使用完整点序列；其他工具用起点+终点
  let cmdPoints;
  if (currentTool === 'pencil' || currentTool === 'eraser' || currentTool === 'mosaic') {
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
      // 涂抹（PixPin 风格）：沿轨迹画圆形笔刷 + 连线，每点取周围平均色。
      // 固定笔刷半径 16px（物理像素），平均色让信息不可辨认 + 笔触有方向性。
      if (cmd.points.length >= 1 && cropImageData) {
        const r = 16;
        c.imageSmoothingEnabled = true;
        for (let i = 0; i < cmd.points.length; i++) {
          const p = cmd.points[i];
          const avg = sampleAverageColor(cropImageData, p.x, p.y, r);
          c.fillStyle = avg;
          c.beginPath();
          c.arc(p.x, p.y, r, 0, Math.PI * 2);
          c.fill();
          // 相邻点之间用线段连接（避免离散圆点留缝）
          if (i > 0) {
            const prev = cmd.points[i - 1];
            c.strokeStyle = avg;
            c.lineWidth = r * 2;
            c.lineCap = 'round';
            c.beginPath();
            c.moveTo(prev.x, prev.y);
            c.lineTo(p.x, p.y);
            c.stroke();
          }
        }
      }
      break;
    case 'pixelate':
      // 经典像素化马赛克（矩形框选）：整个区域分块，每块用平均色填充。
      // blockSize=10，比涂抹更"硬"的遮挡，适合整片打码。
      if (cmd.points.length >= 2 && cropImageData) {
        const [p1, p2] = cmd.points;
        const x = Math.min(p1.x, p2.x);
        const y = Math.min(p1.y, p2.y);
        const w = Math.abs(p2.x - p1.x);
        const h = Math.abs(p2.y - p1.y);
        if (w > 2 && h > 2) {
          drawPixelate(c, cropImageData, x, y, w, h, 10);
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

// ── 涂抹采样辅助 ──────────────────────────────────────

/**
 * 在 ImageData 上采样 (x,y) 周围 r 半径内所有像素的 RGB 平均色。
 * 用于涂抹工具（mosaic）：笔刷点局部平均化让原图信息不可辨认。
 * 坐标系为 ImageData 的像素坐标（物理像素，相对裁剪区左上角）。
 * 返回 `rgba(r,g,b,1)` 字符串。越界像素自动 clamp。
 */
function sampleAverageColor(imageData, x, y, r) {
  const { data, width: iw, height: ih } = imageData;
  let sumR = 0, sumG = 0, sumB = 0, count = 0;
  const x0 = Math.max(0, Math.floor(x - r));
  const x1 = Math.min(iw - 1, Math.ceil(x + r));
  const y0 = Math.max(0, Math.floor(y - r));
  const y1 = Math.min(ih - 1, Math.ceil(y + r));
  const r2 = r * r;
  for (let py = y0; py <= y1; py++) {
    for (let px = x0; px <= x1; px++) {
      // 圆形 mask：只累加圆内像素
      const dx = px - x;
      const dy = py - y;
      if (dx * dx + dy * dy <= r2) {
        const idx = (py * iw + px) * 4;
        sumR += data[idx];
        sumG += data[idx + 1];
        sumB += data[idx + 2];
        count++;
      }
    }
  }
  if (count === 0) return 'rgba(0,0,0,1)';
  return `rgba(${Math.round(sumR / count)},${Math.round(sumG / count)},${Math.round(sumB / count)},1)`;
}

/**
 * 对外暴露的涂抹采样：使用当前 cropImageData（由 reset() 注入）。
 * 供预览阶段调用，与 executeCommand 内的采样逻辑共用一份 ImageData。
 */
export function sampleMosaicColor(x, y, r) {
  if (!cropImageData) return 'rgba(0,0,0,1)';
  return sampleAverageColor(cropImageData, x, y, r);
}

/**
 * 经典像素化马赛克绘制：把 (x,y,w,h) 矩形区域分成 blockSize×blockSize 的网格，
 * 每个网格用该区域内所有像素的 RGB 平均色填充。
 *
 * 与「缩小再放大」算法的差别：块内严格用算术平均（信息完全丢失，更"硬"），
 * 而非 nearest-neighbor（保留某个像素值）。视觉上是经典的方块马赛克。
 *
 * 越界像素自动 clamp 到 ImageData 范围。
 */
function drawPixelate(c, imageData, x, y, w, h, blockSize) {
  const { data, width: iw, height: ih } = imageData;
  c.imageSmoothingEnabled = false;
  for (let by = y; by < y + h; by += blockSize) {
    for (let bx = x; bx < x + w; bx += blockSize) {
      // 当前块的边界（最后一个块可能不足 blockSize）
      const bxEnd = Math.min(bx + blockSize, x + w);
      const byEnd = Math.min(by + blockSize, y + h);
      // clamp 到 ImageData 范围
      const sx0 = Math.max(0, Math.floor(bx));
      const sx1 = Math.min(iw - 1, Math.floor(bxEnd - 1));
      const sy0 = Math.max(0, Math.floor(by));
      const sy1 = Math.min(ih - 1, Math.floor(byEnd - 1));
      let sumR = 0, sumG = 0, sumB = 0, count = 0;
      for (let py = sy0; py <= sy1; py++) {
        for (let px = sx0; px <= sx1; px++) {
          const idx = (py * iw + px) * 4;
          sumR += data[idx];
          sumG += data[idx + 1];
          sumB += data[idx + 2];
          count++;
        }
      }
      if (count === 0) continue;
      const avgR = Math.round(sumR / count);
      const avgG = Math.round(sumG / count);
      const avgB = Math.round(sumB / count);
      c.fillStyle = `rgb(${avgR},${avgG},${avgB})`;
      // 在标注层上绘制方块（坐标即图片像素坐标，因为标注 canvas 与裁剪区对齐）
      c.fillRect(bx, by, bxEnd - bx, byEnd - by);
    }
  }
  c.imageSmoothingEnabled = true;
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