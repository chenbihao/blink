//! 标注引擎（0.11.7-b，0.11.8 加 pixelate，0.11.8-b 加 watermark，0.11.9-a 水印独立图层，0.15.1 配置 store 重构 + TOOL_CAPS + 模式切换）：多种标注 + 撤销 + 颜色/粗细。
//!
//! 标注数据模型：
//! ```typescript
//! interface AnnotationCommand {
//!   type: 'rect' | 'ellipse' | 'arrow' | 'pencil' | 'text' | 'number'
//!       | 'mosaic' | 'pixelate' | 'eraser' | 'blur'
//!       | 'highlight-multiply' | 'highlight-translucent'
//!       | 'spotlight' | 'magnifier';
//!   points: {x: number, y: number}[];  // 物理像素坐标，相对裁剪区左上角
//!   color?: string;
//!   width?: number;
//!   fill?: boolean;
//!   text?: string;
//!   style?: 'solid' | 'dashed';         // 0.15.0：笔画样式
//!   mode?: 'box' | 'brush';            // 0.15.1：框选/画笔模式（supportMode 工具）
//!   textConfig?: {                     // 0.15.2：文字配置（text/number 工具）
//!     fontSize: number, fontFamily: string,
//!     bold: boolean, italic: boolean, shadow: boolean
//!   };
//! }
//! ```
//!
//! - `mosaic` / `pixelate`：框选与连续画笔共用经典像素块马赛克算法
//! - `pixelate`（经典像素化马赛克）：矩形框选 [起点, 终点]，整个区域分块平均色填充
//!
//! **水印**（0.11.9-a 起独立于 commands 栈）：
//!   `watermarkConfig: { text, layout, color, opacity } | null`。
//!   覆盖式配置——同一次 overlay 会话内只保留最后一次 `commitWatermark`，不进撤销栈。
//!   `renderCommandsTo` 每次重绘时把 watermarkConfig 画在最上层。
//!   动机：0.11.8 把水印当 command push 进撤销栈，同一水印文字点两次就叠两层；
//!   多次应用/换保存格式都会视觉上"多层水印"。改为单例配置后天然只有一层。
//!
//! 标注坐标使用**物理像素**（canvas 内部像素）坐标系，与裁剪区像素对齐。
//! 前端鼠标事件 `offsetX/Y` 为 CSS 像素，需乘 `renderScale` 转物理像素。

import { TOOL_CAPS } from './ss-state.js';

/** 当前工具类型 */
let currentTool = 'rect';
/** 当前颜色 */
let currentColor = '#ff0000';
/** 0.15.1：分类配置 store（替代单一 currentWidth）
 *  按工具语义分类——笔画（stroke）/ 画笔（brush）/ 文字（text）/ 效果（effect）
 *  颜色全局共享。 */
const config = {
  color: '#ff0000',
  stroke: { width: 4, style: 'solid' },           // 笔画类：形状/箭头/铅笔
  brush:  { size: 16 },                             // 画笔类：马赛克/模糊/橡皮/高亮
  text:   { fontSize: 24, fontFamily: 'sans-serif', bold: false, italic: false, shadow: false },
  effect: { pixelateBlock: 10, blurIntensity: 8 },  // 效果类
};
/** 0.15.1→fix：per-group 模式记忆（'box' | 'brush'），写入 command。
 *  同组工具（如 mosaic/pixelate/blur 共享 'blur' 组）切换时模式保持一致，
 *  不会「同组切换就变回去」。组名来自 TOOL_CAPS[tool].modeGroup。 */
const groupMode = {
  blur: 'brush',
  eraser: 'brush',
  highlight: 'brush',
};
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
/** 原始裁剪区缓存 canvas（高斯模糊嵌图背景复用） */
let cropSourceCanvas = null;
/** 水印配置（0.11.9-a 起独立于 commands 栈；null = 无水印）
 *  形状: { text, layout, color, opacity } | null */
let watermarkConfig = null;
/** 0.15.9：放大镜倍率（默认 1.3，可由工具栏子菜单切换） */
let magnifierZoom = 1.3;
/**
 * OCR/翻译嵌图图层（0.11.10-h：与水印同为"配置型独立图层"）。
 *
 * 一次会话只保留一份 —— OCR 结果 + 可选的译文,覆盖式配置,不进 commands 撤销栈。
 * mode 决定当前是否画嵌图 + 画的是原文还是译文;两次点[识别]切换 mode/开关同一层。
 *
 * 阶段二 c/d 先用简单占位实现:平均色矩形 + 纯文字（字号自适应留 h 阶段）。
 *
 * 形状:
 *   overlayLayer = {
 *     mode: 'source' | 'translated' | null,   // null = 关闭嵌图（layer 仍存,便于再打开）
 *     lines: [
 *       {
 *         rect: { x, y, w, h },   // 物理像素相对裁剪区（与 word.bounding_rect 同坐标系）
 *         srcText: string,        // 原文
 *         dstText: string | null, // 译文（首次翻译前为 null）
 *         bgColor: string | null, // 采样得到的行背景平均色（h 阶段填充,c/d 先留 null 走默认）
 *       },
 *     ],
 *     bgStrategy: 'average' | 'solid',        // (§2.8) 阶段 i 支持切换
 *     fontScale: number,           // (§2.9 j 微调) 字号缩放系数,默认 1.0
 *     showOriginal: boolean,       // (§2.4 j) 译文模式下叠加半透明原文小字
 *     translationTargetLang: string | null,   // 首次翻译时记录目标语言,重复调用可复用
 *     loading: boolean,            // 0.11.10-k:翻译中状态,在嵌图中心显示 loading 动画
 *   }
 */
let overlayLayer = null;

// H2 优化：loading 动画快照——loading 期间用快照恢复 + 仅重绘 spinner，
// 避免每 50ms 全量重放标注命令 + 逐像素采样。
let _skipLoadingSpinner = false;
let _loadingSnapshot = null;

// M2 优化：Canvas 对象池——避免标注重绘时反复 createElement('canvas') + GC
const _canvasPool = [];
const MAX_POOL_SIZE = 4;

/** M2 优化：从池中获取 canvas，尺寸不匹配时自动 resize。
 *  复用时重置所有关键 canvas 状态——上次调用者可能遗留了 globalCompositeOperation='source-in'
 *  或 filter='blur(...)'，不重置会导致新笔画完全不可见。 */
function acquireCanvas(w, h) {
  const c = _canvasPool.length > 0 ? _canvasPool.pop() : document.createElement('canvas');
  if (c.width !== w || c.height !== h) { c.width = w; c.height = h; }
  const ctx = c.getContext('2d');
  if (ctx) {
    ctx.globalCompositeOperation = 'source-over';
    ctx.globalAlpha = 1.0;
    ctx.filter = 'none';
    ctx.imageSmoothingEnabled = true;
    ctx.clearRect(0, 0, w, h);
  }
  return c;
}

/** M2 优化：归还 canvas 到池中供下次复用 */
function releaseCanvas(c) {
  if (_canvasPool.length < MAX_POOL_SIZE) {
    const ctx = c.getContext('2d');
    if (ctx) {
      // 重置状态，确保下次 acquireCanvas 取出时是干净的
      ctx.globalCompositeOperation = 'source-over';
      ctx.globalAlpha = 1.0;
      ctx.filter = 'none';
      ctx.clearRect(0, 0, c.width, c.height);
    }
    _canvasPool.push(c);
  }
}

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
  cropSourceCanvas = null;
  if (cropImageDataRef) {
    cropSourceCanvas = document.createElement('canvas');
    cropSourceCanvas.width = cropW;
    cropSourceCanvas.height = cropH;
    cropSourceCanvas.getContext('2d').putImageData(cropImageDataRef, 0, 0);
  }
  watermarkConfig = null;  // 0.11.9-a：新选区清水印,防止上一轮残留
  overlayLayer = null;     // 0.11.10-h：新选区清嵌图图层
  if (canvas) {
    canvas.width = cropW;
    canvas.height = cropH;
    ctx.clearRect(0, 0, canvas.width, canvas.height);
  }
}

/**
 * 0.20.6：仅更新裁剪区域数据（选区移动后调），不重置 commands。
 * 选区移动 1px 后裁剪区域改变，马赛克/模糊等依赖 cropImageData 的工具
 * 需要新的底图数据。标注命令保持不变（坐标为相对裁剪区的物理像素）。
 */
export function updateCropData(cropImageDataRef, cropW, cropH) {
  cropImageData = cropImageDataRef;
  if (cropImageDataRef) {
    if (!cropSourceCanvas) {
      cropSourceCanvas = document.createElement('canvas');
    }
    cropSourceCanvas.width = cropW;
    cropSourceCanvas.height = cropH;
    cropSourceCanvas.getContext('2d').putImageData(cropImageDataRef, 0, 0);
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

// 0.15.1：分类配置 getter/setter

/** 按工具的 widthCat 返回对应配置层的数值 */
export function getWidthForTool(tool) {
  const caps = TOOL_CAPS[tool];
  if (!caps || !caps.widthCat) return 0;
  switch (caps.widthCat) {
    case 'stroke': return config.stroke.width;
    case 'brush':  return config.brush.size;
    case 'text':   return config.text.fontSize;
    case 'effect': return config.effect.blurIntensity;  // 0.15.11：统一用 blurIntensity
    default: return 0;
  }
}

export function setStrokeWidth(w) { config.stroke.width = w; }
export function getStrokeWidth() { return config.stroke.width; }
export function setBrushSize(s)   { config.brush.size = s; }
export function getBrushSize()    { return config.brush.size; }
export function setTextConfig(partial) { Object.assign(config.text, partial); }
export function getTextConfig() { return { ...config.text }; }
export function setEffectConfig(partial) { Object.assign(config.effect, partial); }
export function getEffectConfig() { return { ...config.effect }; }

/** 0.15.1→fix：per-group 模式。同组工具共享模式记忆。 */
export function getToolMode(tool) {
  const mg = TOOL_CAPS[tool]?.modeGroup;
  return mg ? (groupMode[mg] || 'brush') : 'brush';
}
export function setToolMode(tool, mode) {
  const mg = TOOL_CAPS[tool]?.modeGroup;
  if (mg) groupMode[mg] = mode;
}

// 0.15.1 兼容包装：旧 setWidth/getWidth
export function setWidth(w) { config.stroke.width = w; }
export function getWidth() { return getWidthForTool(currentTool); }

export function setFill(fill) {
  currentFill = fill;
}

// 0.15.0/0.15.1：笔画样式（实线/虚线），存入 config.stroke.style
export function setStrokeStyle(style) {
  config.stroke.style = style === 'dashed' ? 'dashed' : 'solid';
}

export function getStrokeStyle() {
  return config.stroke.style;
}

export function getFill() {
  return currentFill;
}

// ── 绘制操作 ──────────────────────────────────────────

/** 开始绘制（工具按下时调） */
export function startDraw(x, y) {
  // 0.15.13：单次聚光灯——开始新框选时立即清理上一轮聚光灯
  // 这样预览时不会同时显示新旧两个聚光灯的遮罩
  if (currentTool === 'spotlight') {
    commands = commands.filter(c => c.type !== 'spotlight');
    undoIndex = Math.min(undoIndex, commands.length - 1);
    redrawAll();
  }
  drawStartX = x;
  drawStartY = y;
  currentPoints = [{ x, y }];
  return currentTool;
}

/** 拖拽绘制中 */
export function moveDraw(x, y) {
  // 0.15.1→fix：用 TOOL_CAPS + getToolMode 决定点序列 vs 起点终点
  const caps = TOOL_CAPS[currentTool];
  if (!caps) return;
  const useStream = caps.supportMode
    ? (getToolMode(currentTool) === 'brush')
    : (caps.points === 'stream');
  if (useStream) {
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
  // 0.15.1→fix：用 TOOL_CAPS + getToolMode 决定点序列 vs 起点终点
  const caps = TOOL_CAPS[currentTool] || TOOL_CAPS.select;
  const useStream = caps.supportMode
    ? (getToolMode(currentTool) === 'brush')
    : (caps.points === 'stream');
  const cmdPoints = useStream ? points : [{ x: drawStartX, y: drawStartY }, lastPoint];

  const cmd = {
    type: currentTool,
    points: cmdPoints,
    color: currentColor,
    width: getWidthForTool(currentTool),
    fill: currentFill,
    style: config.stroke.style,  // 0.15.0：笔画样式写入 command
    mode: caps.supportMode ? getToolMode(currentTool) : undefined,  // 0.15.1→fix：模式写入 command
  };

  // 如果是文本工具，需要用户输入文字；通过回调交给主脚本处理
  if (currentTool === 'text') {
    // 保存临时命令，等待文本输入完成
    pendingTextCmd = cmd;
    pendingTextCmd.textConfig = { ...config.text };
    currentPoints = [];
    return { needsText: true, x: drawStartX, y: drawStartY };
  }

  // 0.15.2：数字标号——counter 从 undo 栈实时推算，不单独维护
  if (currentTool === 'number') {
    const counter = commands.slice(0, undoIndex + 1).filter((c) => c.type === 'number').length + 1;
    cmd.text = String(counter);
    cmd.textConfig = { ...config.text };
    commands = commands.slice(0, undoIndex + 1);
    commands.push(cmd);
    undoIndex = commands.length - 1;
    redrawAll();
    currentPoints = [];
    return { needsText: false };
  }

  // 裁剪掉 undoIndex 之后的命令（新命令覆盖重做历史）
  commands = commands.slice(0, undoIndex + 1);
  // 0.15.12：聚光灯（单次）替换旧的；多次聚光灯允许叠加
  if (currentTool === 'spotlight') {
    commands = commands.filter(c => c.type !== 'spotlight');
  }
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
      c.lineWidth = cmd.width || getWidthForTool(cmd.type);
      if (cmd.style === 'dashed' || (cmd.style === undefined && config.stroke.style === 'dashed')) c.setLineDash([8, 4]);
      c.strokeRect(x, y, w, h);
      c.setLineDash([]);
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
        c.lineWidth = cmd.width || getWidthForTool(cmd.type);
        if (cmd.style === 'dashed' || (cmd.style === undefined && config.stroke.style === 'dashed')) c.setLineDash([8, 4]);
        c.beginPath();
        c.ellipse(cx, cy, rx, ry, 0, 0, Math.PI * 2);
        c.stroke();
        c.setLineDash([]);
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
        const headLen = 12 * (cmd.width || getWidthForTool(cmd.type)) / 2;
      c.strokeStyle = cmd.color || currentColor;
      c.lineWidth = cmd.width || getWidthForTool(cmd.type);
      if (cmd.style === 'dashed' || (cmd.style === undefined && config.stroke.style === 'dashed')) c.setLineDash([8, 4]);
      c.beginPath();
      c.moveTo(p1.x, p1.y);
      c.lineTo(p2.x, p2.y);
      c.stroke();
      c.setLineDash([]);
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
        c.lineWidth = cmd.width || getWidthForTool(cmd.type);
        c.lineCap = 'round';
        c.lineJoin = 'round';
        if (cmd.style === 'dashed' || (cmd.style === undefined && config.stroke.style === 'dashed')) c.setLineDash([8, 4]);
        c.beginPath();
        c.moveTo(cmd.points[0].x, cmd.points[0].y);
        for (let i = 1; i < cmd.points.length; i++) {
          c.lineTo(cmd.points[i].x, cmd.points[i].y);
        }
        c.stroke();
        c.setLineDash([]);
      }
      break;
    case 'highlight-multiply':
    case 'highlight-translucent':
      // 0.15.1：box 模式 = 半透明矩形填充；brush 模式 = 现有离屏 stroke 逻辑
      if (cmd.mode === 'box' && cmd.points.length >= 2) {
        const [p1, p2] = cmd.points;
        const x = Math.min(p1.x, p2.x);
        const y = Math.min(p1.y, p2.y);
        const w = Math.abs(p2.x - p1.x);
        const h = Math.abs(p2.y - p1.y);
        const alpha = cmd.type === 'highlight-multiply' ? 0.55 : 0.30;
        c.save();
        c.globalAlpha = alpha;
        c.fillStyle = cmd.color || currentColor;
        c.fillRect(x, y, w, h);
        c.restore();
        break;
      }
      // 荧光笔（0.11.8-c；0.11.8-d 实现"重叠不加深"）：粗线 + 半透明色沿轨迹。
      //
      // 关键难点：单条命令内轨迹自交（例如画 O 或 8）时不能加深。
      // 直接在 c 上 stroke 半透明色，`lineJoin:round` 让**同一次 stroke() 调用内**
      // 的自交处不叠加（Canvas 规范：一次 stroke 是一次原子渲染），但如果轨迹够长
      // 或断点多次，浏览器实际会分批（视具体实现）——为了确定性，两种模式都改成：
      //   1) 先在离屏 canvas 上用**满 alpha 颜色**画整条 polyline
      //   2) 再以目标 alpha `drawImage` 到目标 canvas
      // 这样"这一整笔"作为一层贴上，无论怎么自交都只有一层颜色（multiply 的语义）。
      //
      // 两种模式的差别只是 alpha：multiply 更浓，translucent 更淡。
      // 粗细 = width × 4。
      if (cmd.points.length >= 2 && canvas) {
        const alpha = cmd.type === 'highlight-multiply' ? 0.55 : 0.30;
        const lineW = (cmd.width || config.brush.size) * 4;
        const off = acquireCanvas(canvas.width, canvas.height);
        const offCtx = off.getContext('2d');
        offCtx.strokeStyle = cmd.color || currentColor;
        offCtx.lineWidth = lineW;
        offCtx.lineCap = 'round';
        offCtx.lineJoin = 'round';
        offCtx.beginPath();
        offCtx.moveTo(cmd.points[0].x, cmd.points[0].y);
        for (let i = 1; i < cmd.points.length; i++) {
          offCtx.lineTo(cmd.points[i].x, cmd.points[i].y);
        }
        offCtx.stroke();
        c.save();
        c.globalAlpha = alpha;
        c.drawImage(off, 0, 0);
        c.restore();
        releaseCanvas(off);
      }
      break;
    case 'text':
      // 0.15.2：文字渲染改读 config.text（字号/字体/粗斜阴影）
      if (cmd.text && cmd.points.length >= 1) {
        const p = cmd.points[0];
        const tc = cmd.textConfig || config.text;
        const fontStyle = tc.italic ? 'italic ' : '';
        const fontWeight = tc.bold ? 'bold ' : '';
        c.font = `${fontStyle}${fontWeight}${tc.fontSize}px ${tc.fontFamily}`;
        c.fillStyle = cmd.color || currentColor;
        c.textBaseline = 'top';
        if (tc.shadow) {
          c.shadowColor = 'rgba(0,0,0,0.5)';
          c.shadowBlur = 4;
        }
        c.fillText(cmd.text, p.x, p.y);
        if (tc.shadow) {
          c.shadowColor = 'transparent';
          c.shadowBlur = 0;
        }
      }
      break;
    case 'number':
      // 0.15.12：数字标号——圆形实心底 + 镂空数字，居中对齐鼠标点击位置。
      // 圆形大小跟随 brushSize，数字以反色（白色）居中绘制。
      if (cmd.text && cmd.points.length >= 1 && canvas) {
        const p = cmd.points[0];
        const tc = cmd.textConfig || config.text;
        // 圆形半径基于 brush.size（物理像素）
        const radius = Math.max(10, config.brush.size * 1.2);
        // 0.15.12：圆心 = 点击位置（之前是 p.y + radius 偏下）
        const cx = p.x;
        const cy = p.y;
        // 画实心圆
        c.save();
        c.fillStyle = cmd.color || currentColor;
        c.beginPath();
        c.arc(cx, cy, radius, 0, Math.PI * 2);
        c.fill();
        // 镂空数字：白色文字居中
        const fontSize = Math.max(10, Math.round(radius * 1.1));
        const fontStyle = tc.italic ? 'italic ' : '';
        const fontWeight = tc.bold ? 'bold ' : 'bold ';
        c.font = `${fontStyle}${fontWeight}${fontSize}px ${tc.fontFamily}`;
        c.fillStyle = '#ffffff';
        c.textAlign = 'center';
        c.textBaseline = 'middle';
        c.fillText(cmd.text, cx, cy);
        c.restore();
      }
      break;
    // 注：'watermark' 分支已于 0.11.9-a 移除。水印现走独立 `watermarkConfig`
    // 单例配置,在 renderCommandsTo 末尾统一绘制,不进 commands 栈。
    case 'mosaic':
      // 0.15.11：强度滑块统一控制三种效果——mosaic box 模式用 intensity 作为马赛克块大小
      if (cmd.mode === 'box' && cmd.points.length >= 2 && cropImageData) {
        const [p1, p2] = cmd.points;
        const x = Math.min(p1.x, p2.x);
        const y = Math.min(p1.y, p2.y);
        const w = Math.abs(p2.x - p1.x);
        const h = Math.abs(p2.y - p1.y);
        if (w > 2 && h > 2) {
          const block = Math.max(2, config.effect.blurIntensity);
          drawPixelate(c, cropImageData, x, y, w, h, block);
        }
        break;
      }
      // brush 模式（默认）：复用框选模式的经典像素块算法，再以连续笔迹裁剪。
      if (cmd.points.length >= 1 && cropImageData) {
        const block = Math.max(2, config.effect.blurIntensity);
        const brushW = (cmd.width || config.brush.size) * 2;
        drawPixelateBrush(c, cropImageData, cmd.points, brushW, block);
      } else if (!cropImageData) {
        console.warn('[annot] mosaic: cropImageData 为空，马赛克不可用');
      }
      break;
    case 'pixelate':
      // 0.15.12：马赛克工具合并——画笔与框选共用经典像素块算法，区别仅在覆盖区域。
      if (cmd.mode === 'brush' && cmd.points.length >= 1 && cropImageData) {
        const block = Math.max(2, config.effect.blurIntensity);
        const brushW = (cmd.width || config.brush.size) * 2;
        drawPixelateBrush(c, cropImageData, cmd.points, brushW, block);
        break;
      }
      // box 模式：经典像素化马赛克（矩形框选）
      if (cmd.points.length >= 2 && cropImageData) {
        const [p1, p2] = cmd.points;
        const x = Math.min(p1.x, p2.x);
        const y = Math.min(p1.y, p2.y);
        const w = Math.abs(p2.x - p1.x);
        const h = Math.abs(p2.y - p1.y);
        if (w > 2 && h > 2) {
          const block = Math.max(2, config.effect.blurIntensity);
          drawPixelate(c, cropImageData, x, y, w, h, block);
        }
      }
      break;
    case 'blur':
      // 0.15.3：高斯模糊。box 模式 = 框选区域模糊；brush 模式 = 沿笔画路径模糊。
      // 数据源用 cropSourceCanvas（reset() 缓存的原始裁剪图）。
      if (cmd.mode === 'box' && cmd.points.length >= 2 && cropSourceCanvas) {
        const [p1, p2] = cmd.points;
        const x = Math.min(p1.x, p2.x);
        const y = Math.min(p1.y, p2.y);
        const w = Math.abs(p2.x - p1.x);
        const h = Math.abs(p2.y - p1.y);
        if (w > 2 && h > 2) {
          const intensity = config.effect.blurIntensity;
          c.save();
          c.beginPath();
          c.rect(x, y, w, h);
          c.clip();
          c.filter = `blur(${intensity}px)`;
          c.drawImage(cropSourceCanvas, 0, 0);
          c.filter = 'none';
          c.restore();
        }
        break;
      }
      // brush 模式：离屏 stroke mask + source-in 模糊图。
      if (cmd.points.length >= 1 && cropSourceCanvas && canvas) {
        const intensity = config.effect.blurIntensity;
        const brushW = config.brush.size * 2;
        const off = acquireCanvas(canvas.width, canvas.height);
        const offCtx = off.getContext('2d');
        // 1) 画 stroke mask
        offCtx.strokeStyle = '#fff';
        offCtx.lineWidth = brushW;
        offCtx.lineCap = 'round';
        offCtx.lineJoin = 'round';
        offCtx.beginPath();
        offCtx.moveTo(cmd.points[0].x, cmd.points[0].y);
        for (let i = 1; i < cmd.points.length; i++) {
          offCtx.lineTo(cmd.points[i].x, cmd.points[i].y);
        }
        offCtx.stroke();
        // 2) source-in 保留 mask 区域，贴模糊原图
        offCtx.globalCompositeOperation = 'source-in';
        offCtx.filter = `blur(${intensity}px)`;
        offCtx.drawImage(cropSourceCanvas, 0, 0);
        offCtx.filter = 'none';
        c.drawImage(off, 0, 0);
        releaseCanvas(off);
        break;
      }
      break;
    case 'spotlight':
      // 0.15.3：聚光灯——半透明遮罩 + 镂空选中区。
      // 0.15.11：支持多次聚光灯——改为填充选区外的四条矩形（非 even-odd 全屏），
      // 避免第二个聚光灯的遮罩覆盖第一个聚光灯的镂空区。
      if (cmd.points.length >= 2 && canvas) {
        const [p1, p2] = cmd.points;
        const x = Math.min(p1.x, p2.x);
        const y = Math.min(p1.y, p2.y);
        const w = Math.abs(p2.x - p1.x);
        const h = Math.abs(p2.y - p1.y);
        c.save();
        c.fillStyle = 'rgba(0,0,0,0.6)';
        // 四条遮罩条（选区外的上下左右），不覆盖选区本身
        c.fillRect(0, 0, canvas.width, y);                    // 上
        c.fillRect(0, y + h, canvas.width, canvas.height - y - h); // 下
        c.fillRect(0, y, x, h);                                 // 左
        c.fillRect(x + w, y, canvas.width - x - w, h);        // 右
        c.restore();
      }
      break;
    case 'magnifier':
      // 0.15.12：局部放大——框选区域整体膨胀到 zoom 倍。
      // 选取的 100x100 区域 → 放大为 130x130（zoom=1.3），从框选中心向外膨胀，不裁剪。
      // 数据源用 cropSourceCanvas。
      if (cmd.points.length >= 2 && cropSourceCanvas) {
        const [p1, p2] = cmd.points;
        const x = Math.min(p1.x, p2.x);
        const y = Math.min(p1.y, p2.y);
        const w = Math.abs(p2.x - p1.x);
        const h = Math.abs(p2.y - p1.y);
      if (w > 4 && h > 4) {
        const zoom = magnifierZoom;
        const dw = w * zoom;
        const dh = h * zoom;
        // 从框选中心向外膨胀
        const dx = x + (w - dw) / 2;
        const dy = y + (h - dh) / 2;
        // 绘制放大后的图像（不裁剪，允许溢出框选区域）
          c.save();
          c.drawImage(cropSourceCanvas, x, y, w, h, dx, dy, dw, dh);
          // 边框画在膨胀后的区域
          c.strokeStyle = cmd.color || currentColor;
          c.lineWidth = 2;
          c.strokeRect(dx, dy, dw, dh);
          c.restore();
        }
      }
      break;
    case 'eraser':
      // 0.15.1：box 模式 = 整片 clearRect；brush = 现有沿路径擦除
      if (cmd.mode === 'box' && cmd.points.length >= 2 && c) {
        const [p1, p2] = cmd.points;
        const x = Math.min(p1.x, p2.x);
        const y = Math.min(p1.y, p2.y);
        const w = Math.abs(p2.x - p1.x);
        const h = Math.abs(p2.y - p1.y);
        c.save();
        c.globalCompositeOperation = 'destination-out';
        c.fillRect(x, y, w, h);
        c.restore();
        break;
      }
      // brush 模式（默认）：沿路径圆形擦除
      if (cmd.points.length >= 1 && c) {
        const r = Math.max(6, (cmd.width || config.brush.size) * 3);
        c.save();
        c.globalCompositeOperation = 'destination-out';
        for (let i = 0; i < cmd.points.length; i++) {
          const p = cmd.points[i];
          c.beginPath();
          c.arc(p.x, p.y, r, 0, Math.PI * 2);
          c.fill();
          if (i > 0) {
            const prev = cmd.points[i - 1];
            c.strokeStyle = '#000'; // 颜色无所谓，destination-out 只看 alpha
            c.lineWidth = r * 2;
            c.lineCap = 'round';
            c.beginPath();
            c.moveTo(prev.x, prev.y);
            c.lineTo(p.x, p.y);
            c.stroke();
          }
        }
        c.restore();
      }
      break;
  }
  c.restore();
}

// ── 水印绘制（0.11.8-b）──────────────────────────────────

/**
 * 绘制水印：按 layout 在裁剪区上铺文字。
 *
 * 布局：
 * - `diagonal`：整片对角平铺，-30° 倾斜，网格间距 = 文字宽度 * 2
 * - `top-left/top-right/bottom-left/bottom-right`：四角，距边缘 8% 短边
 * - `top-center/bottom-center`：上下居中
 *
 * 字号：短边 * 0.06（大小随图缩放，保证 300px 缩略图与 4K 全屏视觉重量一致）
 * 颜色：cmd.color + cmd.opacity（0-1，默认 0.35）
 */
function drawWatermark(c, cmd, cw, ch) {
  const short = Math.min(cw, ch);
  const fontSize = Math.max(12, Math.round(short * 0.06));
  const layout = cmd.layout || 'diagonal';
  const opacity = typeof cmd.opacity === 'number' ? cmd.opacity : 0.35;
  // 0.15.12：密度（50-300%，100% = 默认间距，越大越稀疏）
  const density = typeof cmd.density === 'number' ? cmd.density : 1.0;
  const color = withAlpha(cmd.color || '#000000', opacity);
  const text = cmd.text;

  c.save();
  c.font = `${fontSize}px sans-serif`;
  c.fillStyle = color;
  c.textBaseline = 'middle';
  c.textAlign = 'center';

  if (layout === 'diagonal') {
    // 对角平铺：先旋转坐标系，再在旋转后的大 bbox 内网格铺
    const angle = -Math.PI / 6; // -30°
    const metrics = c.measureText(text);
    const tw = metrics.width;
    // 0.15.12：密度影响步长——density 越大间距越大（越稀疏）
    const step = Math.max(tw + fontSize * 3, fontSize * 6) * density;
    // 旋转后需要覆盖的 bbox（对角线长度即可保证不留空）
    const diag = Math.sqrt(cw * cw + ch * ch);
    c.translate(cw / 2, ch / 2);
    c.rotate(angle);
    for (let y = -diag / 2; y < diag / 2; y += step * 0.7) {
      for (let x = -diag / 2; x < diag / 2; x += step) {
        c.fillText(text, x, y);
      }
    }
  } else {
    // 单点布局：四角 + 上下居中
    const pad = Math.max(fontSize * 0.6, short * 0.03);
    let x = cw / 2, y = ch / 2, align = 'center';
    switch (layout) {
      case 'top-left':      x = pad; y = pad + fontSize / 2; align = 'left'; break;
      case 'top-right':     x = cw - pad; y = pad + fontSize / 2; align = 'right'; break;
      case 'bottom-left':   x = pad; y = ch - pad - fontSize / 2; align = 'left'; break;
      case 'bottom-right':  x = cw - pad; y = ch - pad - fontSize / 2; align = 'right'; break;
      case 'top-center':    x = cw / 2; y = pad + fontSize / 2; align = 'center'; break;
      case 'bottom-center': x = cw / 2; y = ch - pad - fontSize / 2; align = 'center'; break;
    }
    c.textAlign = align;
    c.fillText(text, x, y);
  }
  c.restore();
}

/** 把 #rrggbb 或 rgb() 转为带 alpha 的 rgba() 字符串。非法输入 fallback 到黑色。
 *  export 供主脚本预览时复用（保持与引擎最终渲染的 alpha 逻辑一致）。 */
export function withAlpha(color, alpha) {
  if (typeof color !== 'string') return `rgba(0,0,0,${alpha})`;
  const s = color.trim();
  // #rgb / #rrggbb
  if (s[0] === '#') {
    let hex = s.slice(1);
    if (hex.length === 3) hex = hex.split('').map((c) => c + c).join('');
    if (hex.length === 6) {
      const r = parseInt(hex.slice(0, 2), 16);
      const g = parseInt(hex.slice(2, 4), 16);
      const b = parseInt(hex.slice(4, 6), 16);
      return `rgba(${r},${g},${b},${alpha})`;
    }
  }
  // rgb(a) — 用正则拆分，追加 alpha
  // 0.15.8-fix：如果输入已有 alpha，则与目标 alpha 相乘
  const m = s.match(/rgba?\(([^)]+)\)/i);
  if (m) {
    const parts = m[1].split(',').map((p) => p.trim());
    if (parts.length >= 3) {
      const originalAlpha = parts[3] !== undefined ? parseFloat(parts[3]) : 1;
      return `rgba(${parts[0]},${parts[1]},${parts[2]},${(alpha * originalAlpha).toFixed(4)})`;
    }
  }
  return `rgba(0,0,0,${alpha})`;
}

/**
 * 提交/更新水印配置（0.11.9-a：覆盖式,不进 commands 栈）。
 *
 * 语义变更（相对 0.11.8）：
 * - 旧：push 到 commands,能被撤销;同一水印文字点两次叠两层
 * - 新：覆盖式配置,一次会话只有一份水印;不参与撤销/重做
 *
 * 想清除水印走 `clearWatermark()`（对应前端"清除水印"按钮）。
 *
 * 参数缺 text 时视为清除（表单里清空文字再应用 = 清除）。
 */
export function commitWatermark({ text, layout, color, width: _width, opacity, density } = {}) {
  const trimmed = typeof text === 'string' ? text.trim() : '';
  if (!trimmed) {
    watermarkConfig = null;
    redrawAll();
    return;
  }
  watermarkConfig = {
    text: trimmed,
    layout: layout || 'diagonal',
    color: color || currentColor,
    opacity: typeof opacity === 'number' ? opacity : 0.35,
    // 0.15.12：密度 50-300% → 0.5-3.0
    density: typeof density === 'number' ? density : 1.0,
  };
  redrawAll();
}

/** 清除当前水印（供 UI"清除水印"按钮调）。 */
export function clearWatermark() {
  watermarkConfig = null;
  redrawAll();
}

/** 0.15.9：重置所有标注——清空命令栈 + 水印 + 嵌图，不重置 canvas 尺寸/cropData。 */
export function clearAll() {
  commands = [];
  undoIndex = -1;
  watermarkConfig = null;
  overlayLayer = null;
  currentPoints = [];
  pendingTextCmd = null;
  redrawAll();
}

/** 0.15.14：清除所有聚光灯命令（单次↔多次切换时调用） */
export function clearSpotlights() {
  const before = commands.length;
  commands = commands.filter(c => c.type !== 'spotlight' && c.type !== 'spotlight-multi');
  if (commands.length !== before) {
    undoIndex = Math.min(undoIndex, commands.length - 1);
    redrawAll();
  }
}

/** 读取当前水印配置（供 UI 打开表单时回填）。 */
export function getWatermark() {
  return watermarkConfig ? { ...watermarkConfig } : null;
}

/** 是否已配置水印。 */
export function hasWatermark() {
  return watermarkConfig !== null;
}

// ── OCR/翻译嵌图图层 API（0.11.10-h）─────────────────────
//
// 配置型独立图层,不进 commands 栈,与水印并列。
// 两种视图切换（source/translated）复用同一份 lines,只切 mode 属性。

/**
 * 建立/更新嵌图图层数据。
 *
 * 首次点[识别]:传 `{ lines: [{rect, srcText}], mode: 'source' }`
 * 首次点[翻译]:传 `{ lines: [{rect, srcText, dstText}], mode: 'translated', targetLang }`
 * 或先建立 source 再补译文:第二次调用只带 dstText 更新已有 lines。
 *
 * @param {{ lines: Array, mode?: 'source'|'translated'|null, bgStrategy?: string, targetLang?: string|null }} config
 */
export function setOverlay(config = {}) {
  const lines = Array.isArray(config.lines) ? config.lines : [];
  const mode = config.mode === undefined ? 'source' : config.mode;
  // 保留原有 fontScale/showOriginal(若已存在),便于阶段 i/j 局部更新时不丢失。
  const prev = overlayLayer || {};
  const newBgStrategy = config.bgStrategy || 'average';
  // H3 优化：bgStrategy 变化时清除缓存的 bgColor/inkColor，强制重新采样
  const bgStrategyChanged = prev.bgStrategy !== newBgStrategy;
  overlayLayer = {
    mode,
    lines: lines.map((l) => ({
      rect: l.rect,
      srcText: l.srcText || '',
      dstText: l.dstText || null,
      bgColor: bgStrategyChanged ? null : (l.bgColor || null),
      inkColor: bgStrategyChanged ? null : (l.inkColor || null),
    })),
    bgStrategy: newBgStrategy,
    fontScale: typeof config.fontScale === 'number' ? config.fontScale : (prev.fontScale ?? 1.0),
    showOriginal: typeof config.showOriginal === 'boolean' ? config.showOriginal : (prev.showOriginal ?? false),
    translationTargetLang: config.targetLang || null,
  };
  redrawAll();
}

/**
 * 只切换 mode(不换数据源);用户点[识别]↔[翻译] 切换视图时用。
 * 传 null 关闭嵌图但保留 lines(便于再次开启)。
 */
export function setOverlayMode(mode) {
  if (!overlayLayer) return;
  overlayLayer.mode = mode;
  redrawAll();
}

/** 更新已有 overlayLayer 的 dstText(翻译异步完成后回填)。 */
export function setOverlayTranslations(dstTexts, targetLang) {
  if (!overlayLayer || !Array.isArray(dstTexts)) return;
  const n = Math.min(dstTexts.length, overlayLayer.lines.length);
  for (let i = 0; i < n; i++) {
    overlayLayer.lines[i].dstText = dstTexts[i];
  }
  overlayLayer.translationTargetLang = targetLang || overlayLayer.translationTargetLang;
  redrawAll();
}

/** 更新原文但保留 overlay 的其它显示配置；面板校对后同步时使用。 */
export function setOverlaySourceTexts(srcTexts) {
  if (!overlayLayer || !Array.isArray(srcTexts)) return;
  const n = Math.min(srcTexts.length, overlayLayer.lines.length);
  for (let i = 0; i < n; i++) {
    overlayLayer.lines[i].srcText = srcTexts[i];
    // 原文变化后旧译文不再可信，要求下次切译文时重新翻译。
    overlayLayer.lines[i].dstText = null;
  }
  overlayLayer.translationTargetLang = null;
  redrawAll();
}

/** 0.11.10-j:调整嵌图字号缩放系数(0.6-1.4)。 */
export function setOverlayFontScale(scale) {
  if (!overlayLayer) return;
  const clamped = Math.max(0.4, Math.min(2.0, Number(scale) || 1.0));
  overlayLayer.fontScale = clamped;
  redrawAll();
}

/** 0.11.10-j:切换"译文模式下同时显示原文小字"的对照 toggle。 */
export function setOverlayShowOriginal(flag) {
  if (!overlayLayer) return;
  overlayLayer.showOriginal = !!flag;
  redrawAll();
}

/** 0.11.10-k:设置翻译中 loading 状态。翻译中在嵌图中心显示 loading 动画。 */
export function setOverlayLoading(loading) {
  if (!overlayLayer) return;
  overlayLayer.loading = !!loading;
  // H2 优化：loading 结束时清除快照
  if (!loading) {
    _loadingSnapshot = null;
  }
  redrawAll();
}

/** H2 优化：仅重绘 loading 动画——恢复快照 + 画 spinner，替代全量 redrawAnnotFull */
export function redrawLoadingSpinner() {
  if (!ctx || !canvas || !_loadingSnapshot || !overlayLayer || !overlayLayer.loading) return;
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(_loadingSnapshot, 0, 0);
  drawLoadingSpinner(ctx, overlayLayer);
}

/** 清空嵌图图层(整片下线)。 */
export function clearOverlay() {
  overlayLayer = null;
  redrawAll();
}

/**
 * 将显式传入的 overlay 快照渲染到目标 ctx。
 *
 * 用于「翻译并 Pin」后台合成链路——任务自己持有 overlay 副本，
 * 不依赖即将被清理的全局 overlayLayer 或 annotCanvas。
 *
 * 要求：
 * - 使用传入的 overlaySnapshot；
 * - 不读取模块全局 overlayLayer；
 * - 不修改当前编辑会话；
 * - 可复用现有内部 drawOverlay 实现。
 *
 * 注意：drawOverlay 内部的背景采样会读取模块全局 cropImageData/cropSourceCanvas，
 * 这属于图片底层数据（非编辑会话状态），在会话未被 reset 前仍然有效。
 * 若 bgColor/inkColor 已在 overlaySnapshot.lines 中缓存，则不会触发采样。
 */
export function renderOverlaySnapshotTo(overlaySnapshot, targetCtx, width, height) {
  if (!overlaySnapshot || !overlaySnapshot.mode) return;
  drawOverlay(targetCtx, overlaySnapshot, width, height);
}

/** 只读快照(供 UI 判断 / 面板召唤时读文本)。 */
export function getOverlay() {
  return overlayLayer ? {
    mode: overlayLayer.mode,
    lines: overlayLayer.lines.map((l) => ({ ...l, rect: { ...l.rect } })),
    bgStrategy: overlayLayer.bgStrategy,
    fontScale: overlayLayer.fontScale ?? 1.0,
    showOriginal: !!overlayLayer.showOriginal,
    translationTargetLang: overlayLayer.translationTargetLang,
  } : null;
}

/** overlayLayer 存在且当前 mode 非 null（有真实内容显示中）。 */
export function isOverlayActive() {
  return overlayLayer !== null && overlayLayer.mode !== null;
}

/** 是否配置了 overlay(哪怕 mode=null)——用于面板召唤条件判断。 */
export function hasOverlay() {
  return overlayLayer !== null;
}

/** 0.11.10-k:overlay 是否处于翻译中 loading 状态。 */
export function isOverlayLoading() {
  return overlayLayer !== null && !!overlayLayer.loading;
}

/** 0.15.6：获取裁剪区原始 canvas（供配色提取等模块复用） */
export function getCropSourceCanvas() {
  return cropSourceCanvas;
}

/** 0.15.9：放大镜倍率 getter/setter */
export function getMagnifierZoom() { return magnifierZoom; }
export function setMagnifierZoom(z) { magnifierZoom = Math.max(1.1, Math.min(4.0, z)); }

// H7 优化：复用的临时小 canvas（drawPixelate 用缩小再放大替代逐像素循环）
let _pixelateTempCanvas = null;

/**
 * 经典像素化马赛克绘制：把 (x,y,w,h) 矩形区域分成 blockSize×blockSize 的网格，
 * 每个网格用该区域内所有像素的 RGB 平均色填充。
 *
 * H7 优化：用 drawImage 缩小再放大替代逐像素 JS 循环——
 * 缩小时 GPU 双线性插值 ≈ 算术平均，放大时关闭平滑产生方块效果。
 * 性能：O(w*h) 像素读取 → 2 次 drawImage（GPU 加速）。
 */
function drawPixelate(c, imageData, x, y, w, h, blockSize) {
  // H7 优化：优先用 cropSourceCanvas 做 drawImage 缩放（GPU 加速）
  if (cropSourceCanvas) {
    const bw = Math.max(1, Math.ceil(w / blockSize));
    const bh = Math.max(1, Math.ceil(h / blockSize));
    if (!_pixelateTempCanvas) {
      _pixelateTempCanvas = document.createElement('canvas');
    }
    _pixelateTempCanvas.width = bw;
    _pixelateTempCanvas.height = bh;
    const tempCtx = _pixelateTempCanvas.getContext('2d');
    if (tempCtx) {
      // 缩小：双线性插值做块内平均
      tempCtx.imageSmoothingEnabled = true;
      tempCtx.drawImage(cropSourceCanvas, x, y, w, h, 0, 0, bw, bh);
      // 放大：关闭平滑产生方块效果
      c.imageSmoothingEnabled = false;
      c.drawImage(_pixelateTempCanvas, 0, 0, bw, bh, x, y, w, h);
      c.imageSmoothingEnabled = true;
      return;
    }
  }
  // fallback：无 cropSourceCanvas 时退回原始逐像素循环
  drawPixelateSlow(c, imageData, x, y, w, h, blockSize);
}

/** 原始逐像素马赛克算法（fallback，无 cropSourceCanvas 时用）。 */
function drawPixelateSlow(c, imageData, x, y, w, h, blockSize) {
  const { data, width: iw, height: ih } = imageData;
  c.imageSmoothingEnabled = false;
  for (let by = y; by < y + h; by += blockSize) {
    for (let bx = x; bx < x + w; bx += blockSize) {
      const bxEnd = Math.min(bx + blockSize, x + w);
      const byEnd = Math.min(by + blockSize, y + h);
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
      c.fillRect(bx, by, bxEnd - bx, byEnd - by);
    }
  }
  c.imageSmoothingEnabled = true;
}

/** 在当前上下文绘制一条不透明、连续、圆角的画笔遮罩。 */
function drawBrushMask(c, points, width) {
  c.fillStyle = '#fff';
  c.strokeStyle = '#fff';
  c.lineWidth = width;
  c.lineCap = 'round';
  c.lineJoin = 'round';
  if (points.length === 1) {
    c.beginPath();
    c.arc(points[0].x, points[0].y, width / 2, 0, Math.PI * 2);
    c.fill();
    return;
  }
  c.beginPath();
  c.moveTo(points[0].x, points[0].y);
  for (let i = 1; i < points.length; i++) {
    c.lineTo(points[i].x, points[i].y);
  }
  c.stroke();
}

/**
 * 用连续笔迹裁剪经典像素块马赛克。
 * 画笔宽度只决定覆盖范围；blockSize 只决定方块大小，二者互不耦合。
 */
function drawPixelateBrush(c, imageData, points, brushWidth, blockSize) {
  if (points.length === 0 || !c.canvas) return;
  const block = Math.max(2, Math.round(blockSize));
  const radius = brushWidth / 2;
  let minX = points[0].x;
  let maxX = points[0].x;
  let minY = points[0].y;
  let maxY = points[0].y;
  for (let i = 1; i < points.length; i++) {
    minX = Math.min(minX, points[i].x);
    maxX = Math.max(maxX, points[i].x);
    minY = Math.min(minY, points[i].y);
    maxY = Math.max(maxY, points[i].y);
  }
  // 边界对齐到全图网格，保证同一笔和相邻多笔的马赛克块不会随采样点漂移。
  const x0 = Math.max(0, Math.floor((minX - radius - 1) / block) * block);
  const y0 = Math.max(0, Math.floor((minY - radius - 1) / block) * block);
  const x1 = Math.min(imageData.width, Math.ceil((maxX + radius + 1) / block) * block);
  const y1 = Math.min(imageData.height, Math.ceil((maxY + radius + 1) / block) * block);
  if (x1 <= x0 || y1 <= y0) return;

  const off = acquireCanvas(c.canvas.width, c.canvas.height);
  const offCtx = off.getContext('2d');
  const pixelated = acquireCanvas(c.canvas.width, c.canvas.height);
  const pixelatedCtx = pixelated.getContext('2d');
  if (!offCtx || !pixelatedCtx) { releaseCanvas(off); releaseCanvas(pixelated); return; }

  drawBrushMask(offCtx, points, brushWidth);
  drawPixelate(pixelatedCtx, imageData, x0, y0, x1 - x0, y1 - y0, block);
  offCtx.globalCompositeOperation = 'source-in';
  offCtx.drawImage(pixelated, 0, 0);
  c.drawImage(off, 0, 0);
  releaseCanvas(off);
  releaseCanvas(pixelated);
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

/** 全量重绘标注层（0.11.8-e 加正片叠底单独图层聚合）
 *
 * highlight-multiply 特殊处理："同颜色多笔画不加深" —— 按颜色分组，每颜色一个
 * 离屏 canvas，同色多笔画都 source-over 到同一个 offscreen（无 alpha 累积因为
 * 满 alpha 颜色），最后一次性 alpha drawImage 到主 canvas。
 * 跨颜色仍会叠加（合理，红黄叠出橙感）。
 * highlight-translucent 保持原逐笔 alpha drawImage（"半透明"语义就是"多笔会加深"）。
 */
function redrawAll() {
  if (!ctx || !canvas) return;
  // H2 优化：loading 期间跳过 spinner 绘制，先渲染快照（含文字/标注），再画 spinner
  const isLoading = overlayLayer && overlayLayer.loading && overlayLayer.mode === 'translated';
  if (isLoading) _skipLoadingSpinner = true;
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  renderCommandsTo(commands.slice(0, undoIndex + 1), ctx, canvas.width, canvas.height);
  if (isLoading) {
    _skipLoadingSpinner = false;
    updateLoadingSnapshot();
    drawLoadingSpinner(ctx, overlayLayer);
  }
}

/**
 * 将命令序列渲染到目标 ctx，尊重 highlight-multiply "同色不加深"语义。
 *
 * export 供主脚本 redrawAnnotFull 和合成阶段复用——**必须走这个**而非手动 loop
 * executeCommand，否则 highlight-multiply 会退化成"逐笔 alpha 累积"。
 *
 * 0.11.9-a：末尾统一绘制 `watermarkConfig`（若有）。水印是"最后一层",
 * 永远在最上;保存/合成时也走 renderCommandsTo,水印天然只有一层。
 */
export function renderCommandsTo(cmds, targetCtx, w, h) {
  const highlightMultiplyCmds = [];
  const spotlightMultiCmds = [];
  for (const cmd of cmds) {
    if (cmd.type === 'highlight-multiply') {
      highlightMultiplyCmds.push(cmd);
    } else if (cmd.type === 'spotlight-multi') {
      // 0.15.12：多次聚光灯收集到组，统一渲染为单层遮罩（叠底只应用一次）
      spotlightMultiCmds.push(cmd);
    } else {
      executeCommand(cmd, targetCtx);
    }
  }
  if (highlightMultiplyCmds.length > 0) {
    renderHighlightMultiplyLayer(highlightMultiplyCmds, targetCtx, w, h);
  }
  // 0.15.12：多次聚光灯——单层遮罩，重叠区域暗度不叠加
  if (spotlightMultiCmds.length > 0 && w > 0 && h > 0) {
    renderSpotlightMultiLayer(spotlightMultiCmds, targetCtx, w, h);
  }
  // 0.11.10-h：OCR/翻译嵌图在水印之前（水印永远最上层）
  if (overlayLayer && overlayLayer.mode) {
    drawOverlay(targetCtx, overlayLayer, w, h);
  }
  // 0.11.9-a：水印永远画在最上层（不进 commands 栈,一次会话只一层）
  if (watermarkConfig) {
    drawWatermark(targetCtx, watermarkConfig, w, h);
  }
}

/** 把所有 highlight-multiply 命令按颜色分组渲染，每组一个 offscreen 满 alpha stroke，
 *  然后按颜色出现的先后顺序 alpha drawImage 到目标 ctx。 */
function renderHighlightMultiplyLayer(cmds, targetCtx, w, h) {
  const alpha = 0.55;
  // 按颜色分组，保留每组第一次出现的顺序
  const colorOrder = [];
  const layers = new Map(); // color -> offscreen canvas
  for (const cmd of cmds) {
    if (!cmd.points || cmd.points.length < 2) continue;
    const color = cmd.color || currentColor;
    let layer = layers.get(color);
    if (!layer) {
      layer = document.createElement('canvas');
      layer.width = w;
      layer.height = h;
      layers.set(color, layer);
      colorOrder.push(color);
    }
    const lc = layer.getContext('2d');
    lc.strokeStyle = color;
    lc.lineWidth = (cmd.width || config.brush.size) * 4;
    lc.lineCap = 'round';
    lc.lineJoin = 'round';
    lc.beginPath();
    lc.moveTo(cmd.points[0].x, cmd.points[0].y);
    for (let i = 1; i < cmd.points.length; i++) {
      lc.lineTo(cmd.points[i].x, cmd.points[i].y);
    }
    lc.stroke();
  }
  // 按插入顺序贴回目标 ctx
  for (const color of colorOrder) {
    targetCtx.save();
    targetCtx.globalAlpha = alpha;
    targetCtx.drawImage(layers.get(color), 0, 0);
    targetCtx.restore();
  }
}

/** 0.15.12：多次聚光灯——单层遮罩渲染。
 *  离屏 canvas：先填满 rgba(0,0,0,0.6)，然后 clearRect 所有聚光灯区域，
 *  最后一次性 drawImage 到目标 ctx。重叠聚光灯的暗度不叠加（叠底只应用一次）。
 *  0.15.13：clearRect 替代 destination-out+fillRect，更可靠地镂空区域。 */
function renderSpotlightMultiLayer(cmds, targetCtx, w, h) {
  const off = acquireCanvas(w, h);
  const offCtx = off.getContext('2d');
  // 填满遮罩
  offCtx.fillStyle = 'rgba(0,0,0,0.6)';
  offCtx.fillRect(0, 0, w, h);
  // 镂空所有聚光灯区域（clearRect 更可靠——destination-out 对半透明像素可能残留）
  for (const cmd of cmds) {
    if (cmd.points.length >= 2) {
      const [p1, p2] = cmd.points;
      const x = Math.min(p1.x, p2.x);
      const y = Math.min(p1.y, p2.y);
      const rw = Math.abs(p2.x - p1.x);
      const rh = Math.abs(p2.y - p1.y);
      offCtx.clearRect(x, y, rw, rh);
    }
  }
  // 绘制到目标
  targetCtx.drawImage(off, 0, 0);
  releaseCanvas(off);
}

// ── 输出 ──────────────────────────────────────────────

/** 获取当前标注命令列表（序列化用） */
export function getCommands() {
  return commands.slice(0, undoIndex + 1);
}

/** 是否有标注（含水印/嵌图,供合成阶段判断是否需要贴 annot layer） */
export function hasAnnotations() {
  return commands.length > 0
    || watermarkConfig !== null
    || (overlayLayer !== null && overlayLayer.mode !== null);
}

// ── OCR/翻译嵌图渲染（0.11.10-h：图层引擎真本事）─────────────────
//
// 输入:overlayLayer.lines + cropImageData(裁剪区原始像素,resize 时存的)。
// 输出:每行画背景遮罩 + 字号自适应文字。
//
// 策略:
// - 背景色:采样 rect 周围环形环带(rect.h 高向外扩 4px)的像素,忽略暗色"字色"像素,
//          剩余取 RGB 平均;若采样为空退化到白色。
// - 字色:根据背景亮度阈值 128 切换深/浅字体(深底→浅字/浅底→深字)。
// - 字号:起始 rect.h * 1.0,迭代减 1 直到 measureText.width <= rect.w * 0.95,
//        下限 8px;若仍超宽二分找最长前缀 + 省略号。
//        相邻行字号偏差超 25% 视为有意不同层级(标题/正文),分组各自均值统一。
//
// 所有采样/字号算法都是纯函数(见文件末尾的 `sample*` / `fitFontSize` / `luminance`),
// 便于后续在 Node 环境 mock ctx 做单测。

/** H2 优化：绘制 loading 动画（旋转弧线）。从 drawOverlay 提取为独立函数，
 *  供 redrawLoadingSpinner() 快速重绘 spinner 而无需全量重放标注命令。 */
function drawLoadingSpinner(targetCtx, layer) {
  if (!layer.lines || layer.lines.length === 0) return;
  // 计算所有 lines 的包围盒
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const line of layer.lines) {
    const r = line.rect;
    if (!r || r.w <= 0 || r.h <= 0) continue;
    minX = Math.min(minX, r.x);
    minY = Math.min(minY, r.y);
    maxX = Math.max(maxX, r.x + r.w);
    maxY = Math.max(maxY, r.y + r.h);
  }
  if (minX < maxX && minY < maxY) {
    const cx = (minX + maxX) / 2;
    const cy = (minY + maxY) / 2;
    const radius = 18;
    // 半透明背景圆
    targetCtx.beginPath();
    targetCtx.arc(cx, cy, radius + 8, 0, Math.PI * 2);
    targetCtx.fillStyle = 'rgba(0, 0, 0, 0.55)';
    targetCtx.fill();
    // 旋转弧线（用时间驱动角度）
    const t = (Date.now() % 1200) / 1200;
    const startAngle = t * Math.PI * 2;
    const endAngle = startAngle + Math.PI * 1.2;
    targetCtx.beginPath();
    targetCtx.arc(cx, cy, radius, startAngle, endAngle);
    targetCtx.strokeStyle = '#4a9eff';
    targetCtx.lineWidth = 3;
    targetCtx.lineCap = 'round';
    targetCtx.stroke();
  }
}

/** H2 优化：将当前 annotCanvas（不含 spinner）复制到快照 canvas */
function updateLoadingSnapshot() {
  if (!_loadingSnapshot) {
    _loadingSnapshot = document.createElement('canvas');
  }
  if (_loadingSnapshot.width !== canvas.width || _loadingSnapshot.height !== canvas.height) {
    _loadingSnapshot.width = canvas.width;
    _loadingSnapshot.height = canvas.height;
  }
  const snapCtx = _loadingSnapshot.getContext('2d');
  snapCtx.clearRect(0, 0, canvas.width, canvas.height);
  snapCtx.drawImage(canvas, 0, 0);
}

function drawOverlay(targetCtx, layer, _w, _h) {
  if (!layer.lines || layer.lines.length === 0) return;
  const mode = layer.mode;
  const bgStrategy = layer.bgStrategy || 'average';
  const fontScale = layer.fontScale ?? 1.0;
  const showOriginal = !!layer.showOriginal;
  const isLoading = !!layer.loading;

  targetCtx.save();

  // 0.11.10-k：翻译中 loading 动画——在嵌图区域中心绘制
  // H2 优化：loading 动画提取为独立函数，redrawAll 跳过它以构建快照
  if (isLoading && mode === 'translated' && !_skipLoadingSpinner) {
    drawLoadingSpinner(targetCtx, layer);
    // loading 态仍继续画文字（显示原文作为占位）
  }

  // ── Pass 1: 预算每行字号,按偏差分组取中位数统一 ──
  // 同段文字各行 rect.h 基本一致,但宽度适配会让长行字号更小;
  // 相邻行字号跳变超过阈值视为有意不同层级(标题/正文),分组各自统一。
  // 用中位数替代均值,天然抗异常行干扰;分组锚点用组内中位数而非首元素。
  const lineEntries = [];  // { line, r, text }
  const rawSizes = [];     // 与 lineEntries 一一对应,每行独立 fitFontSize 结果
  for (const line of layer.lines) {
    const r = line.rect;
    if (!r || r.w <= 0 || r.h <= 0) continue;
    const text = mode === 'translated' ? line.dstText : line.srcText;
    if (!text) continue;
    lineEntries.push({ line, r, text });
    const { size } = fitFontSize(targetCtx, text, r, fontScale);
    rawSizes.push(size > 0 ? size : 0);
  }
  if (lineEntries.length === 0) {
    targetCtx.restore();
    return;
  }

  // 分组:相邻行字号偏差超过 25% 则断开,视为不同层级
  // 锚点用组内中位数,比首元素更稳定
  const GROUP_THRESHOLD = 0.25;
  const groups = [];  // [{start, end, medianSize}]
  let gStart = 0;
  const medianOf = (arr) => {
    if (arr.length === 0) return 0;
    const sorted = arr.slice().sort((a, b) => a - b);
    const mid = Math.floor(sorted.length / 2);
    return sorted.length % 2 !== 0 ? sorted[mid] : Math.round((sorted[mid - 1] + sorted[mid]) / 2);
  };

  for (let i = 1; i <= rawSizes.length; i++) {
    // 用组内已有行的中位数做锚点
    const groupSizes = rawSizes.slice(gStart, i).filter((s) => s > 0);
    const anchor = medianOf(groupSizes);
    const shouldBreak = i === rawSizes.length
      || rawSizes[i] === 0
      || anchor === 0
      || Math.abs(rawSizes[i] - anchor) / anchor > GROUP_THRESHOLD;
    if (shouldBreak) {
      const cleanSizes = rawSizes.slice(gStart, i).filter((s) => s > 0);
      groups.push({ start: gStart, end: i, medianSize: medianOf(cleanSizes) });
      gStart = i;
    }
  }

  // 为每行分配所属组的统一字号(中位数)
  const unifiedSizes = new Array(lineEntries.length).fill(0);
  for (const g of groups) {
    for (let i = g.start; i < g.end; i++) {
      unifiedSizes[i] = g.medianSize;
    }
  }

  for (let i = 0; i < lineEntries.length; i++) {
    const { line, r, text } = lineEntries[i];
    // ── 背景 ──
    // blur 直接把原图对应区域模糊绘回；其它策略再用色块覆盖。
    // H3 优化：bgColor/inkColor 首次计算后缓存到 line 对象，后续重绘直接读取
    let backgroundDrawn = false;
    let bg = line.bgColor;
    if (!bg) {
      if (bgStrategy === 'solid') {
        bg = 'rgba(255, 255, 255, 0.92)';
      } else if (bgStrategy === 'blur') {
        backgroundDrawn = drawBlurredBackground(targetCtx, r);
        if (!backgroundDrawn) bg = 'rgba(255, 255, 255, 0.92)';
      } else {
        // 平均色策略(默认):采样 rect 周围环形区
        bg = sampleAverageBackgroundColor(r) || 'rgba(255, 255, 255, 0.95)';
      }
      // 缓存到 line 对象（blur 策略不缓存——它是绘制操作而非纯色值）
      if (bg && bgStrategy !== 'blur') {
        line.bgColor = bg;
      }
    }
    if (!backgroundDrawn) {
      targetCtx.fillStyle = bg;
      targetCtx.fillRect(r.x, r.y, r.w, r.h);
    }

    // ── 文字 ──

    // 字色：优先采样原图文字颜色（匹配原文字色），失败时用背景亮度决定深/浅
    // H3 优化：inkColor 缓存到 line；sampleInkColor 接收 bg 参数避免重复采样
    let ink = line.inkColor;
    if (!ink) {
      ink = sampleInkColor(r, bg) || pickInkColorByBg(bg || 'rgba(255, 255, 255, 0.95)');
      if (ink) {
        line.inkColor = ink;
      }
    }
    // 组内统一字号 + 重新计算截断/省略
    const fontPx = unifiedSizes[i];
    const display = fitDisplayText(targetCtx, text, r, fontPx);
    if (fontPx <= 0) continue;

    targetCtx.fillStyle = ink;
    targetCtx.font = `${fontPx}px system-ui, "Microsoft YaHei", "Noto Sans SC", sans-serif`;
    targetCtx.textBaseline = 'middle';
    targetCtx.textAlign = 'left';
    targetCtx.fillText(display, r.x + 2, r.y + r.h / 2);

    // ── 保留原文对照(0.11.10-j)──
    // 仅在译文模式 + showOriginal 打开 + 有 srcText 时叠加,画在 rect 顶部小字
    if (mode === 'translated' && showOriginal && line.srcText && line.srcText !== text) {
      const smallPx = Math.max(8, Math.floor(fontPx * 0.55));
      targetCtx.save();
      targetCtx.globalAlpha = 0.55;
      targetCtx.fillStyle = ink;
      targetCtx.font = `${smallPx}px system-ui, "Microsoft YaHei", "Noto Sans SC", sans-serif`;
      targetCtx.textBaseline = 'top';
      // 简单截断:测长后 slice
      let orig = line.srcText;
      const maxW = r.w - 4;
      if (targetCtx.measureText(orig).width > maxW) {
        while (orig.length > 2 && targetCtx.measureText(orig + '…').width > maxW) {
          orig = orig.slice(0, -1);
        }
        orig = orig + '…';
      }
      targetCtx.fillText(orig, r.x + 2, r.y + 1);
      targetCtx.restore();
    }
  }
  targetCtx.restore();
}

// ── 背景采样(0.11.10-h §2.8)─────────────────────────

/**
 * 采样 rect 周围环形带的像素平均色(忽略字色暗像素)。
 *
 * 环形带 = 以 rect 为核心,向外扩 4px 的边框区域(内圈是 rect 本身)。
 * 忽略字色的启发式:先算带内所有像素的平均亮度,亮度低于 平均值 - 20 的像素视为"字色",
 * 不参与最终 RGB 平均。这样文字周围环带上"字延伸出去的部分"不会污染背景色。
 *
 * 空采样(rect 挨着裁剪区边缘导致带完全在外)返回 null,调用方 fallback。
 */
function sampleAverageBackgroundColor(rect) {
  if (!cropImageData) return null;
  const { data, width: iw, height: ih } = cropImageData;
  const margin = 4;
  const x0 = Math.max(0, Math.floor(rect.x - margin));
  const x1 = Math.min(iw - 1, Math.ceil(rect.x + rect.w + margin));
  const y0 = Math.max(0, Math.floor(rect.y - margin));
  const y1 = Math.min(ih - 1, Math.ceil(rect.y + rect.h + margin));
  const rx0 = Math.max(0, Math.floor(rect.x));
  const rx1 = Math.min(iw - 1, Math.ceil(rect.x + rect.w));
  const ry0 = Math.max(0, Math.floor(rect.y));
  const ry1 = Math.min(ih - 1, Math.ceil(rect.y + rect.h));

  // Pass 1:收集环带所有像素 + 记录亮度
  const samples = [];   // {r,g,b,lum}
  let sumLum = 0;
  for (let py = y0; py <= y1; py++) {
    for (let px = x0; px <= x1; px++) {
      // 只要环形带内(排除 rect 内部)
      if (px >= rx0 && px <= rx1 && py >= ry0 && py <= ry1) continue;
      const idx = (py * iw + px) * 4;
      const r = data[idx], g = data[idx + 1], b = data[idx + 2];
      const l = luminance(r, g, b);
      samples.push({ r, g, b, l });
      sumLum += l;
    }
  }
  if (samples.length === 0) return null;
  const avgLum = sumLum / samples.length;
  const inkThreshold = avgLum - 20;

  // Pass 2:忽略字色像素,累加剩余
  let sumR = 0, sumG = 0, sumB = 0, count = 0;
  for (const s of samples) {
    if (s.l < inkThreshold) continue;
    sumR += s.r; sumG += s.g; sumB += s.b; count++;
  }
  if (count === 0) {
    // 所有像素都被判定为"字色" → 极小概率,退化用全体平均
    for (const s of samples) { sumR += s.r; sumG += s.g; sumB += s.b; }
    count = samples.length;
  }
  const R = Math.round(sumR / count);
  const G = Math.round(sumG / count);
  const B = Math.round(sumB / count);
  return `rgba(${R}, ${G}, ${B}, 0.95)`;
}

/**
 * 高斯模糊策略：从缓存的原始裁剪图扩大取样，再裁回目标行区域。
 * 扩大 6px 可避免 blur 边缘透明；目标 ctx 的 save/restore 保证 filter 不外泄。
 */
function drawBlurredBackground(targetCtx, rect) {
  if (!cropSourceCanvas) return false;
  const blurPx = 4;
  const pad = blurPx * 2;
  const sx = Math.max(0, Math.floor(rect.x - pad));
  const sy = Math.max(0, Math.floor(rect.y - pad));
  const sx2 = Math.min(cropSourceCanvas.width, Math.ceil(rect.x + rect.w + pad));
  const sy2 = Math.min(cropSourceCanvas.height, Math.ceil(rect.y + rect.h + pad));
  const sw = sx2 - sx;
  const sh = sy2 - sy;
  if (sw <= 0 || sh <= 0) return false;

  targetCtx.save();
  targetCtx.beginPath();
  targetCtx.rect(rect.x, rect.y, rect.w, rect.h);
  targetCtx.clip();
  targetCtx.filter = `blur(${blurPx}px)`;
  targetCtx.drawImage(cropSourceCanvas, sx, sy, sw, sh, sx, sy, sw, sh);
  targetCtx.filter = 'none';
  targetCtx.fillStyle = 'rgba(255, 255, 255, 0.08)';
  targetCtx.fillRect(rect.x, rect.y, rect.w, rect.h);
  targetCtx.restore();
  return true;
}

// ── 字号自适应 + 字色选择(0.11.10-h §2.9)────────────

/**
 * 计算 sRGB 相对亮度(0-255 范围,近似 ITU-R BT.709)。
 * 用于:1) 背景采样时判断"字色暗像素"  2) 决定字色深浅。
 */
function luminance(r, g, b) {
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

/** 从 CSS 颜色字符串抽 rgb;不支持的格式返回 null。用于背景色 → 字色反查。 */
function parseRgb(css) {
  const m = /rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)/i.exec(css);
  if (!m) return null;
  return { r: +m[1], g: +m[2], b: +m[3] };
}

/** 根据背景色选深/浅字色(阈值 128)。 */
function pickInkColorByBg(bgCss) {
  const rgb = parseRgb(bgCss);
  if (!rgb) return '#111';
  return luminance(rgb.r, rgb.g, rgb.b) > 128 ? '#111' : '#f5f5f5';
}

/**
 * 采样 rect 内部的文字颜色（用于嵌字时匹配原文字色）。
 *
 * 算法：采样背景色，然后取高对比度的字色。
 * - 深色背景 → 白色字
 * - 浅色背景 → 黑色字
 *
 * @returns {string|null} CSS 颜色字符串，采样失败返回 null
 */
function sampleInkColor(rect, bgCss) {
  // H3 优化：接收已采样的 bg 参数，避免重复调用 sampleAverageBackgroundColor
  const bg = bgCss || sampleAverageBackgroundColor(rect);
  if (!bg) return null;
  return pickInkColorByBg(bg);
}

/**
 * 迭代找到能塞进 rect.w * 0.95 的最大字号。
 *
 * 起始 rect.h * 0.85 * fontScale,每次 -1 逐步尝试(rect 高度一般不超过 60px,循环上限 ~50 次可控)。
 * 到达下限 8px 仍超宽 → 用 8px + 二分找最长前缀 + 省略号截断。
 * rect 极窄(连一个字都放不下 8px)→ 返回 size=0 让 drawOverlay 跳过。
 *
 * @param {number} fontScale - 用户在面板里指定的字号缩放系数(0.4-2.0),默认 1.0
 */
function fitFontSize(ctx, text, rect, fontScale = 1.0) {
  const maxWidth = rect.w * 0.95;
  const minSize = 8;
  let size = Math.max(minSize, Math.floor(rect.h * 1.0 * fontScale));
  ctx.save();
  ctx.font = `${size}px system-ui, "Microsoft YaHei", "Noto Sans SC", sans-serif`;
  while (size > minSize && ctx.measureText(text).width > maxWidth) {
    size -= 1;
    ctx.font = `${size}px system-ui, "Microsoft YaHei", "Noto Sans SC", sans-serif`;
  }
  if (ctx.measureText(text).width <= maxWidth) {
    ctx.restore();
    return { size, display: text };
  }
  // 到 minSize 仍超宽 → 截断 + 省略号
  const ellipsis = '…';
  const ellW = ctx.measureText(ellipsis).width;
  if (ellW > maxWidth) {
    ctx.restore();
    return { size: 0, display: '' };   // rect 极窄,一个省略号都放不下
  }
  let lo = 0, hi = text.length;
  while (lo < hi) {
    const mid = Math.floor((lo + hi + 1) / 2);
    if (ctx.measureText(text.slice(0, mid)).width + ellW <= maxWidth) lo = mid;
    else hi = mid - 1;
  }
  ctx.restore();
  return { size: minSize, display: text.slice(0, lo) + ellipsis };
}

/**
 * 字号已确定时,计算文字在 rect 内的显示文本(超宽则截断+省略号)。
 * 与 fitFontSize 的截断逻辑相同,但不调整字号。
 */
function fitDisplayText(ctx, text, rect, fontSize) {
  const maxWidth = rect.w * 0.95;
  ctx.save();
  ctx.font = `${fontSize}px system-ui, "Microsoft YaHei", "Noto Sans SC", sans-serif`;
  if (ctx.measureText(text).width <= maxWidth) {
    ctx.restore();
    return text;
  }
  const ellipsis = '…';
  const ellW = ctx.measureText(ellipsis).width;
  if (ellW > maxWidth) {
    ctx.restore();
    return '';
  }
  let lo = 0, hi = text.length;
  while (lo < hi) {
    const mid = Math.floor((lo + hi + 1) / 2);
    if (ctx.measureText(text.slice(0, mid)).width + ellW <= maxWidth) lo = mid;
    else hi = mid - 1;
  }
  ctx.restore();
  return text.slice(0, lo) + ellipsis;
}
