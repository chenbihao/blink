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

import {TOOL_CAPS} from './ss-state.js';

/** 当前工具类型 */
let currentTool = 'rect';
/** 当前颜色 */
let currentColor = '#ff0000';
/** 0.15.1：分类配置 store（替代单一 currentWidth）
 *  按工具语义分类——笔画（stroke）/ 画笔（brush）/ 文字（text）/ 效果（effect）
 *  颜色全局共享。 */
const config = {
    color: '#ff0000',
    stroke: {width: 4, style: 'solid'},           // 笔画类：形状/箭头/铅笔
    brush: {size: 16},                             // 画笔类：马赛克/模糊/橡皮/高亮
    text: {fontSize: 24, fontFamily: 'sans-serif', bold: false, italic: false, shadow: false},
    effect: {pixelateBlock: 10, blurIntensity: 8},  // 效果类
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
    if (c.width !== w || c.height !== h) {
        c.width = w;
        c.height = h;
    }
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
        case 'stroke':
            return config.stroke.width;
        case 'brush':
            return config.brush.size;
        case 'text':
            return config.text.fontSize;
        case 'effect':
            return config.effect.blurIntensity;  // 0.15.11：统一用 blurIntensity
        default:
            return 0;
    }
}

export function setStrokeWidth(w) {
    config.stroke.width = w;
}

export function getStrokeWidth() {
    return config.stroke.width;
}

export function setBrushSize(s) {
    config.brush.size = s;
}

export function getBrushSize() {
    return config.brush.size;
}

export function setTextConfig(partial) {
    Object.assign(config.text, partial);
}

export function getTextConfig() {
    return {...config.text};
}

export function setEffectConfig(partial) {
    Object.assign(config.effect, partial);
}

export function getEffectConfig() {
    return {...config.effect};
}

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
export function setWidth(w) {
    config.stroke.width = w;
}

export function getWidth() {
    return getWidthForTool(currentTool);
}

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
    currentPoints = [{x, y}];
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
        currentPoints.push({x, y});
    }
}

/** 获取当前绘制中的点序列（供主脚本实时预览用）。 */
export function getCurrentPoints() {
    return currentPoints;
}

/** 结束绘制，生成 AnnotationCommand */
export function endDraw(x, y) {
    const points = [...currentPoints];
    const lastPoint = {x, y};
    // 0.15.1→fix：用 TOOL_CAPS + getToolMode 决定点序列 vs 起点终点
    const caps = TOOL_CAPS[currentTool] || TOOL_CAPS.select;
    const useStream = caps.supportMode
        ? (getToolMode(currentTool) === 'brush')
        : (caps.points === 'stream');
    const cmdPoints = useStream ? points : [{x: drawStartX, y: drawStartY}, lastPoint];

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
        pendingTextCmd.textConfig = {...config.text};
        currentPoints = [];
        return {needsText: true, x: drawStartX, y: drawStartY};
    }

    // 0.15.2：数字标号——counter 从 undo 栈实时推算，不单独维护
    if (currentTool === 'number') {
        const counter = commands.slice(0, undoIndex + 1).filter((c) => c.type === 'number').length + 1;
        cmd.text = String(counter);
        cmd.textConfig = {...config.text};
        commands = commands.slice(0, undoIndex + 1);
        commands.push(cmd);
        undoIndex = commands.length - 1;
        redrawAll();
        currentPoints = [];
        return {needsText: false};
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
    return {needsText: false};
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
            case 'top-left':
                x = pad;
                y = pad + fontSize / 2;
                align = 'left';
                break;
            case 'top-right':
                x = cw - pad;
                y = pad + fontSize / 2;
                align = 'right';
                break;
            case 'bottom-left':
                x = pad;
                y = ch - pad - fontSize / 2;
                align = 'left';
                break;
            case 'bottom-right':
                x = cw - pad;
                y = ch - pad - fontSize / 2;
                align = 'right';
                break;
            case 'top-center':
                x = cw / 2;
                y = pad + fontSize / 2;
                align = 'center';
                break;
            case 'bottom-center':
                x = cw / 2;
                y = ch - pad - fontSize / 2;
                align = 'center';
                break;
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
export function commitWatermark({text, layout, color, width: _width, opacity, density} = {}) {
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
    _loadingSnapshot = null;
    _skipLoadingSpinner = false;
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
    return watermarkConfig ? {...watermarkConfig} : null;
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
            fontH: l.fontH ?? null,
            srcText: l.srcText || '',
            dstText: l.dstText || null,
            bgColor: bgStrategyChanged ? null : (l.bgColor || null),
            inkColor: bgStrategyChanged ? null : (l.inkColor || null),
            inkSampled: bgStrategyChanged ? null : (l.inkSampled || null),
            blurBase: bgStrategyChanged ? null : (l.blurBase || null),
            charRects: Array.isArray(l.charRects) ? l.charRects : null,
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
    _loadingSnapshot = null;
    _skipLoadingSpinner = false;
    redrawAll();
}

// ── OCR 结果 → overlay 行映射（0.23.12 自 ss-ocr.js 移入，跨窗口共用）──────

/**
 * 提取后端下发的字号参考高度（`OcrLine.font_height`，JSON 字段 `font_h`）。
 *
 * PP-OCR det 框经 unclip 外扩后 rect.h 大于实际字形高度，后端按经验系数
 * 折减后随行下发；仅用于嵌图字号推导，rect 本身不动。WinRT 路径不下发
 * 该字段（词框 union 已紧贴字形）→ 返回 null，渲染回退 rect.h。
 */
export function toFontH(ln) {
    return (ln && Number.isFinite(ln.font_h) && ln.font_h > 0) ? ln.font_h : null;
}

/**
 * OCR 结果 → overlay 行数组（识别/翻译嵌图/静默划词/pin 翻译覆盖共用）。
 *
 * 行过滤 + 字段映射之外，把 `char_boxes`（逐字符框，X 向紧贴字形）按
 * `line_index` 聚合为每行的 `charRects`，供字色采样把候选像素收紧到字形
 * 内部、几何排除行内非文字干扰（下划线/图标/底纹）。
 * 注意 char_boxes.line_index 对应 result.lines 的**原始下标**；行过滤后
 * 下标会前移，需按原始下标建映射后再归位。
 */
export function toOverlayLines(result) {
    const rawLines = (result && Array.isArray(result.lines)) ? result.lines : [];
    const charBoxes = (result && Array.isArray(result.char_boxes)) ? result.char_boxes : [];
    const out = [];
    const origToOverlay = new Map();
    rawLines.forEach((ln, origIdx) => {
        if (!(ln && ln.text && ln.rect && ln.rect.w > 0 && ln.rect.h > 0)) return;
        origToOverlay.set(origIdx, out.length);
        out.push({
            rect: {x: ln.rect.x, y: ln.rect.y, w: ln.rect.w, h: ln.rect.h},
            fontH: toFontH(ln),
            srcText: ln.text,
        });
    });
    for (const cb of charBoxes) {
        const overlayIdx = (cb && Number.isInteger(cb.line_index))
            ? origToOverlay.get(cb.line_index) : undefined;
        if (overlayIdx === undefined) continue;
        if (!(cb.rect && cb.rect.w > 0 && cb.rect.h > 0)) continue;
        const line = out[overlayIdx];
        if (!line.charRects) line.charRects = [];
        line.charRects.push({x: cb.rect.x, y: cb.rect.y, w: cb.rect.w, h: cb.rect.h});
    }
    return out;
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
        lines: overlayLayer.lines.map((l) => ({...l, rect: {...l.rect}})),
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

/** 供颜色分析只读扫描原始选区像素；调用方不得修改 data。 */
export function getCropImageData() {
    return cropImageData;
}

/** 0.15.9：放大镜倍率 getter/setter */
export function getMagnifierZoom() {
    return magnifierZoom;
}

export function setMagnifierZoom(z) {
    magnifierZoom = Math.max(1.1, Math.min(4.0, z));
}

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
    const {data, width: iw, height: ih} = imageData;
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
    if (!offCtx || !pixelatedCtx) {
        releaseCanvas(off);
        releaseCanvas(pixelated);
        return;
    }

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
// - 背景色:采样 rect 周围环形环带,以通道中位数为中心做稳健平均,
//          对深底浅字/浅底深字对称;若采样为空退化到白色。
// - 字色:优先采样 rect 内原图像素的真实字色(蓝链接/红警告/灰提示等,见
//        sampleOriginalInkColors),有 charRects 时收紧到字形内部并采出逐字
//        符色;原文可按 buildInkSegments 分段着色,译文只采用高置信整行主色;
//        采不到或对比度不足时回退背景对比色(深底→浅字/浅底→深字)。
// - 字号:起始(行 fontH ?? rect.h) * fontScale,迭代减 1 直到
//        measureText.width <= rect.w * 0.95,下限 8px;若仍超宽二分找最长前缀 + 省略号。
//        fontH 是后端折减的字号参考高度(PP-OCR det 框 unclip 外扩,rect.h 偏大;
//        WinRT 路径无 fontH → 直接用 rect.h)。相邻行字号偏差超 25%
//        视为有意不同层级(标题/正文),分组各自均值统一。
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
        lineEntries.push({line, r, text});
        // PP-OCR det 框含 unclip 外扩，rect.h 偏大；fontH 是后端折减后的字号
        // 参考高度（WinRT 无此字段 → 回退 rect.h）。字号推导用 fontRect，
        // 背景覆盖仍用原 rect（见下方 fillRect）。
        const fontRect = (line.fontH && line.fontH > 0 && line.fontH < r.h)
            ? {...r, h: line.fontH} : r;
        const {size} = fitFontSize(targetCtx, text, fontRect, fontScale);
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
            groups.push({start: gStart, end: i, medianSize: medianOf(cleanSizes)});
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
        const {line, r, text} = lineEntries[i];
        // ── 背景 ──
        // blur = 垫底平均色抹掉原文字 + 叠半透明模糊原图保留背景质感；
        // 其它策略用色块覆盖。
        // H3 优化：bgColor/inkColor 首次计算后缓存到 line 对象，后续重绘直接读取
        let backgroundDrawn = false;
        let bg = line.bgColor;
        if (!bg) {
            if (bgStrategy === 'solid') {
                bg = 'rgba(255, 255, 255, 0.92)';
            } else if (bgStrategy === 'blur') {
                backgroundDrawn = drawBlurredBackground(targetCtx, r, line);
                if (!backgroundDrawn) bg = 'rgba(255, 255, 255, 0.92)';
            } else {
                // 平均色策略(默认):采样 rect 周围环形区
                bg = sampleAverageBackgroundColor(r) || 'rgba(255, 255, 255, 0.95)';
            }
            // 缓存到 line 对象（blur 的模糊层是绘制操作不缓存，
            // 但其垫底色在 drawBlurredBackground 内经 line.blurBase 缓存）
            if (bg && bgStrategy !== 'blur') {
                line.bgColor = bg;
            }
        }
        if (!backgroundDrawn) {
            targetCtx.fillStyle = bg;
            targetCtx.fillRect(r.x, r.y, r.w, r.h);
        }

        // ── 文字 ──

        // 字色：原文模式保留真实原文字色；译文模式只接受高置信度的整行主色。
        // 翻译会改变语序与长度，逐字符颜色按长度比例映射既无语义依据，也会把
        // 壁纸/图标/抗锯齿噪声放大成“五颜六色”的译文。
        let sampled = line.inkSampled;
        let sourceInk = line.inkColor;
        const fallbackBg = bg || line.blurBase || null;
        const fallbackInk = sampleInkColor(r, fallbackBg)
            || pickInkColorByBg(fallbackBg || 'rgba(255, 255, 255, 0.95)');
        if (!sampled && !sourceInk) {
            sampled = sampleOriginalInkColors(r, bg, line.charRects);
            if (sampled) line.inkSampled = sampled;
        }
        if (!sourceInk) {
            sourceInk = (sampled && sampled.ink) || fallbackInk;
            line.inkColor = sourceInk;
        }
        const ink = mode === 'translated'
            ? chooseStableTranslatedInk(sampled, fallbackInk, fallbackBg)
            : sourceInk;
        // 组内统一字号 + 重新计算截断/省略
        const fontPx = unifiedSizes[i];
        const display = fitDisplayText(targetCtx, text, r, fontPx);
        if (fontPx <= 0) continue;

        targetCtx.font = `${fontPx}px system-ui, "Microsoft YaHei", "Noto Sans SC", sans-serif`;
        targetCtx.textBaseline = 'middle';
        targetCtx.textAlign = 'left';
        // 逐字符分色对原文总是启用（按词对齐消采样抖动）；译文模式仅对
        // "代码特征行"启用——代码翻译语序近似保留，比例映射的色系区间误差
        // 可接受，换来语法色保留（否则 chooseStableTranslatedInk 的共识门槛
        // 会把多色代码行全部打回黑白灰）。非代码行译文仍用稳定的整行主色。
        const segments = mode === 'source'
            ? buildInkSegments(
                (line.srcText || '').length, display,
                line.inkSampled
                    ? alignInkColorsToWords(line.srcText || '', line.inkSampled.charInks)
                    : null, ink)
            : (mode === 'translated' && line.inkSampled
                && isCodeLikeInkLayout(line.inkSampled, bg)
                ? buildTranslatedInkSegments(line, display, ink)
                : null);
        if (segments) {
            let cursorX = r.x + 2;
            for (const seg of segments) {
                targetCtx.fillStyle = seg.color;
                targetCtx.fillText(seg.text, cursorX, r.y + r.h / 2);
                cursorX += targetCtx.measureText(seg.text).width;
            }
        } else {
            const textX = r.x + 2;
            const textY = r.y + r.h / 2;
            targetCtx.fillStyle = ink;
            targetCtx.fillText(display, textX, textY);
        }

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
 * 采样 rect 周围环形带的稳健背景色。
 *
 * 环形带 = 以 rect 为核心,向外扩 4px 的边框区域(内圈是 rect 本身)。
 * 对 R/G/B 分别取中位数作为背景中心，再只平均与中心足够接近的像素。
 * 相比旧版“只排除暗像素”，该方法对浅底深字和深底浅字对称，白字阴影、
 * 抗锯齿亮边及少量彩色图标都不会单向拉偏背景色。
 *
 * 空采样(rect 挨着裁剪区边缘导致带完全在外)返回 null,调用方 fallback。
 */
export function sampleAverageBackgroundColorFromPixels(imageData, rect) {
    if (!imageData || !imageData.data || !rect) return null;
    const {data, width: iw, height: ih} = imageData;
    const margin = 4;
    const x0 = Math.max(0, Math.floor(rect.x - margin));
    const x1 = Math.min(iw - 1, Math.ceil(rect.x + rect.w + margin));
    const y0 = Math.max(0, Math.floor(rect.y - margin));
    const y1 = Math.min(ih - 1, Math.ceil(rect.y + rect.h + margin));
    const rx0 = Math.max(0, Math.floor(rect.x));
    const rx1 = Math.min(iw - 1, Math.ceil(rect.x + rect.w));
    const ry0 = Math.max(0, Math.floor(rect.y));
    const ry1 = Math.min(ih - 1, Math.ceil(rect.y + rect.h));

    const samples = [];
    for (let py = y0; py <= y1; py++) {
        for (let px = x0; px <= x1; px++) {
            // 只要环形带内(排除 rect 内部)
            if (px >= rx0 && px <= rx1 && py >= ry0 && py <= ry1) continue;
            const idx = (py * iw + px) * 4;
            const r = data[idx], g = data[idx + 1], b = data[idx + 2];
            samples.push({r, g, b});
        }
    }
    if (samples.length === 0) return null;

    const median = (values) => {
        const sorted = values.slice().sort((a, b) => a - b);
        const mid = Math.floor(sorted.length / 2);
        return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
    };
    const center = {
        r: median(samples.map((s) => s.r)),
        g: median(samples.map((s) => s.g)),
        b: median(samples.map((s) => s.b)),
    };
    const maxDist2 = 72 * 72;
    let inliers = samples.filter((s) => rgbDist2(s, center) <= maxDist2);
    // 高纹理背景可能没有明显紧簇；此时中位数本身比全量均值更抗离群点。
    if (inliers.length < Math.max(4, samples.length * 0.2)) inliers = [center];
    let sumR = 0, sumG = 0, sumB = 0;
    for (const s of inliers) {
        sumR += s.r;
        sumG += s.g;
        sumB += s.b;
    }
    const count = inliers.length;
    const R = Math.round(sumR / count);
    const G = Math.round(sumG / count);
    const B = Math.round(sumB / count);
    return `rgba(${R}, ${G}, ${B}, 0.95)`;
}

function sampleAverageBackgroundColor(rect) {
    return sampleAverageBackgroundColorFromPixels(cropImageData, rect);
}

/**
 * 高斯模糊策略：大半径模糊原图铺底，区域性还原背景，细笔画文字被高频滤除。
 *
 * 高斯模糊保低频(背景色块/渐变,几十到几百 px 尺度)压高频(1-3px 文字笔画)，
 * 半径越大这个分离越彻底：12px 下原文残影只剩低对比云雾，而同行内不同底色的
 * 区域差异几乎完整保留——这就是"区域性"的来源。垫底平均色仅 15% 权重，
 * 轻微统一色调并进一步压残影，不破坏区域感（此前垫底 100% 导致整行一个色）。
 * 扩大 2×半径取样可避免 blur 边缘透明；目标 ctx 的 save/restore 保证 filter 不外泄。
 */
function drawBlurredBackground(targetCtx, rect, line) {
    if (!cropSourceCanvas) return false;
    const blurPx = 12;
    const pad = blurPx * 2;
    const sx = Math.max(0, Math.floor(rect.x - pad));
    const sy = Math.max(0, Math.floor(rect.y - pad));
    const sx2 = Math.min(cropSourceCanvas.width, Math.ceil(rect.x + rect.w + pad));
    const sy2 = Math.min(cropSourceCanvas.height, Math.ceil(rect.y + rect.h + pad));
    const sw = sx2 - sx;
    const sh = sy2 - sy;
    if (sw <= 0 || sh <= 0) return false;

    // 垫底色按行缓存（blurBase 独立于 bgColor——调用方读 bgColor 判定是否已处理，
    // 复用会让 blur 分支被跳过退化成纯色填充）
    let base = line.blurBase;
    if (!base) {
        base = sampleAverageBackgroundColor(rect) || 'rgba(255, 255, 255, 0.95)';
        line.blurBase = base;
    }

    targetCtx.save();
    targetCtx.beginPath();
    targetCtx.rect(rect.x, rect.y, rect.w, rect.h);
    targetCtx.clip();
    targetCtx.fillStyle = base;
    targetCtx.fillRect(rect.x, rect.y, rect.w, rect.h);
    targetCtx.globalAlpha = 0.85;
    targetCtx.filter = `blur(${blurPx}px)`;
    targetCtx.drawImage(cropSourceCanvas, sx, sy, sw, sh, sx, sy, sw, sh);
    targetCtx.filter = 'none';
    targetCtx.globalAlpha = 1;
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

/** 从 CSS rgb/rgba/#rgb/#rrggbb 颜色字符串抽 rgb；不支持的格式返回 null。 */
function parseRgb(css) {
    const m = /rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)/i.exec(css);
    if (m) return {r: +m[1], g: +m[2], b: +m[3]};
    const hex = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(String(css || '').trim());
    if (!hex) return null;
    const raw = hex[1].length === 3
        ? hex[1].split('').map((c) => c + c).join('') : hex[1];
    return {
        r: parseInt(raw.slice(0, 2), 16),
        g: parseInt(raw.slice(2, 4), 16),
        b: parseInt(raw.slice(4, 6), 16),
    };
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
 * 0.23.x 起为**回退路径**：真实原文字色采样见 sampleOriginalInkColor，
 * 本函数在其采不到/对比度不足时兜底。
 *
 * @returns {string|null} CSS 颜色字符串，采样失败返回 null
 */
function sampleInkColor(rect, bgCss) {
    // H3 优化：接收已采样的 bg 参数，避免重复调用 sampleAverageBackgroundColor
    const bg = bgCss || sampleAverageBackgroundColor(rect);
    if (!bg) return null;
    return pickInkColorByBg(bg);
}

// ── 原文字色采样(0.23.x)─────────────────────────────

/** 与背景基准的 RGB 距离(欧氏,0-441)超过此值视为文字像素。 */
const INK_DIST_THRESHOLD = 60;
/** 文字候选中 RGB 距离最远的前 30% 视为字形核心(滤掉抗锯齿半混色边缘)。 */
const INK_CORE_RATIO = 0.3;
/** 亮度中位数簇半径:与中位数亮度差 ≤ 20 的像素视为背景。 */
const INK_BG_CLUSTER_LUM = 20;
/** 候选占比下限对应的最小绝对像素数(避免大 rect 下阈值过严)。 */
const INK_MIN_CANDIDATES = 4;
/** 候选占比下限:低于视为"没采到文字"(纯色块/噪声)。 */
const INK_MIN_CANDIDATE_RATIO = 0.02;
/** 候选占比上限:过高说明背景基准翻转或区域杂乱,采样不可信。 */
const INK_MAX_CANDIDATE_RATIO = 0.7;
/** 接受采样字色的最低 WCAG 对比度(对实际绘制背景)。
 *  取 2.5 而非 3.0:浅灰次要文字(#999 对白底 ≈ 2.85)是合法原色,应保留。 */
const INK_MIN_CONTRAST = 2.5;
/** 框外背景像素低于此数时退回整行中位数簇单基准(字符框几乎铺满整行的极端密度)。 */
const INK_BG_PIXEL_MIN = 30;
/** 背景色簇上限:行内同时存在的背景色(徽章内底+外围底等)通常 ≤ 2-3 种。 */
const INK_BG_CLUSTER_MAX = 4;

// ── 译文模式代码行分段着色(0.23.x)─────────────────────────────

/** charInks 覆盖率下限：低于此视为非文字密集行(图标/壁纸行的 charInks 大量为 null)。 */
const INK_CODE_COVERAGE_MIN = 0.7;
/** 结构簇占比下限：低于此的簇是采样碎片，不计入结构色。 */
const INK_CODE_CLUSTER_RATIO_MIN = 0.15;
/** 结构簇数下限：少于 2 无分段意义(整行同色走 fillText 快路径)。 */
const INK_CODE_CLUSTERS_MIN = 2;
/** 结构簇数上限：更多簇是图标彩边/照片纹理的杂色，分段会涂成花斑。 */
const INK_CODE_CLUSTERS_MAX = 3;
/** 结构簇亮度上限：更亮的簇是白色文字主体/浅彩抗锯齿边，不是结构色；
 *  真语法色(蓝 #5ba2f3 / 紫 #b476ae / 橙 #c07349 / 黄绿 #b97c53)亮度均 < 170。 */
const INK_CODE_CLUSTER_LUM_MAX = 205;
/** 碎片段占比上限：低于此的段视为采样噪声段，并入相邻长段。 */
const INK_FRAGMENT_RATIO_MAX = 0.08;
/** 碎片段并入邻段的最大 RGB 色距：超出视为真语法色保留。 */
const INK_FRAGMENT_MERGE_DIST = 120;

/**
 * WCAG 相对亮度对比度(1.0-21.0)。与 luminance() 的区别:luminance() 是
 * 0-255 线性 BT.709 近似(用于阈值判断),此处按 WCAG 公式做 gamma 展开,
 * 用于字色可读性校验。
 */
export function wcagContrastRatio(rgbA, rgbB) {
    const relLum = (c) => {
        const f = (v) => {
            v /= 255;
            return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
        };
        return 0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b);
    };
    const l1 = relLum(rgbA);
    const l2 = relLum(rgbB);
    const [hi, lo] = l1 > l2 ? [l1, l2] : [l2, l1];
    return (hi + 0.05) / (lo + 0.05);
}

/**
 * 从行 rect 内原图像素采样真实字色(嵌字时匹配原文字色)。
 *
 * 背景:pickInkColorByBg 只按背景亮度切黑白,彩色原文(蓝链接/红警告/灰提示)
 * 嵌字后会失真。本函数从像素估计真实字色:
 *
 * 1. 背景基准 = rect 内亮度中位数簇(±INK_BG_CLUSTER_LUM)均值——文字像素
 *    占行框比例低,中位数天然落在背景上;
 * 2. 文字候选 = 与基准 RGB 距离 > INK_DIST_THRESHOLD 的像素;
 * 3. 候选字色 = 候选中距离最远的前 INK_CORE_RATIO(字形核心)均值,
 *    避免抗锯齿半混色边缘把颜色洗淡;
 * 4. 兜底:候选过少/过多,或与实际绘制背景对比度 < INK_MIN_CONTRAST → null,
 *    调用方回退黑白字色。
 *
 * 字色识别只依赖原图像素(自估背景),不信任 drawnBgCss——solid 策略的白色
 * 会把深色底整片误判成"字"。drawnBgCss 仅用于对比度校验(字实际画在什么上);
 * blur 策略无绘制背景色,传 null,此时用背景基准代替。
 *
 * charRects(逐字符框,PP-OCR char_boxes 聚合)把文字候选池收紧到字形内部:
 * 背景基准仍取整行 rect(行框内背景占多数,中位数稳),但候选像素只从字符框
 * 里找——行内非文字干扰(下划线/图标/底纹)被几何排除。字符框全部越界时
 * 回退整行采样。WinRT 等无 char_boxes 的路径不传,维持整行行为。
 *
 * @param {{data: Uint8ClampedArray, width: number, height: number}|null} imageData - 裁剪区像素
 * @param {{x: number, y: number, w: number, h: number}} rect - 行包围盒(物理像素)
 * @param {string|null} drawnBgCss - 实际绘制在字下的背景色
 * @param {Array<{x: number, y: number, w: number, h: number}>|null} [charRects] - 逐字符框并集
 * @returns {string|null} 'rgb(r, g, b)';null = 采不到/不可信
 */
export function sampleOriginalInkColorFromPixels(imageData, rect, drawnBgCss, charRects) {
    const s = sampleOriginalInkColorsFromPixels(imageData, rect, drawnBgCss, charRects);
    return s ? s.ink : null;
}

/**
 * 逐字符颜色采样下限:字符框内文字候选像素少于此数(细窄字符/标点)不单独立色,
 * 继承整行字色。
 */
const INK_CHAR_MIN_CANDIDATES = 3;

/** 单背景基准:亮度中位数簇(±INK_BG_CLUSTER_LUM)均值(无字符框几何时的原有逻辑)。 */
function medianClusterBg(pixels) {
    const lums = pixels.map((p) => p.l);
    lums.sort((a, b) => a - b);
    const medLum = lums[Math.floor(lums.length / 2)];
    let r = 0, g = 0, b = 0, n = 0;
    for (const p of pixels) {
        if (Math.abs(p.l - medLum) <= INK_BG_CLUSTER_LUM) {
            r += p.r;
            g += p.g;
            b += p.b;
            n++;
        }
    }
    return n === 0 ? null : {r: r / n, g: g / n, b: b / n};
}

/** 贪心颜色聚类:像素并入距离 ≤ tol 的最近簇(增量均值),返回原始簇(未按大小排序)。 */
function greedyColorClusters(pixels, tol) {
    const tol2 = tol * tol;
    const clusters = [];
    for (const p of pixels) {
        let best = null;
        for (const cl of clusters) {
            const d2 = (p.r - cl.sr / cl.n) ** 2 + (p.g - cl.sg / cl.n) ** 2 + (p.b - cl.sb / cl.n) ** 2;
            if (d2 <= tol2 && (!best || d2 < best.d2)) best = {cl, d2};
        }
        if (best) {
            best.cl.sr += p.r;
            best.cl.sg += p.g;
            best.cl.sb += p.b;
            best.cl.n++;
        } else {
            clusters.push({sr: p.r, sg: p.g, sb: p.b, n: 1});
        }
    }
    return clusters;
}

/** 背景多色聚类:贪心聚类后按像素数取前 maxK 簇(带 n 供"框内外占比"判别)。 */
function clusterBgColors(pixels, tol, maxK) {
    return greedyColorClusters(pixels, tol)
        .sort((a, b) => b.n - a.n)
        .slice(0, maxK)
        .map((cl) => ({r: cl.sr / cl.n, g: cl.sg / cl.n, b: cl.sb / cl.n, n: cl.n}));
}

/**
 * 行级 + 逐字符字色采样(sampleOriginalInkColorFromPixels 的完整版)。
 *
 * 算法同上,额外地:charRects 收紧路径下,文字候选按所属字符框分桶,
 * 每桶独立求均值与对比度校验 → charInks[i](null = 该字符无足够候选,
 * 继承整行字色)。charInks 与 charRects/srcText 字符一一对应,
 * 供混合色行(如"红色警告:普通文本")分段着色。
 *
 * @returns {{ink: string|null, charInks: Array<string|null>|null, inkConfidence: number}|null}
 *   charInks 为 null 表示非字符框收紧路径(整行采样,无逐字符信息)；
 *   inkConfidence 是字形核心最大颜色簇占比(0-1)。
 */
export function sampleOriginalInkColorsFromPixels(imageData, rect, drawnBgCss, charRects) {
    if (!imageData || !imageData.data) return null;
    const {data, width: iw, height: ih} = imageData;

    const clampRect = (r) => ({
        x0: Math.max(0, Math.floor(r.x)),
        y0: Math.max(0, Math.floor(r.y)),
        x1: Math.min(iw - 1, Math.ceil(r.x + r.w) - 1),
        y1: Math.min(ih - 1, Math.ceil(r.y + r.h) - 1),
    });
    const collect = (b) => {
        const pxs = [];
        for (let py = b.y0; py <= b.y1; py++) {
            const rowBase = py * iw;
            for (let px = b.x0; px <= b.x1; px++) {
                const idx = (rowBase + px) * 4;
                const p = {r: data[idx], g: data[idx + 1], b: data[idx + 2]};
                p.l = luminance(p.r, p.g, p.b);
                p.key = rowBase + px;
                pxs.push(p);
            }
        }
        return pxs;
    };

    // Pass 1:行内全部像素(背景基准) + 字符框内像素(文字候选池,带所属字符索引)
    const lineBox = clampRect(rect);
    if (lineBox.x1 < lineBox.x0 || lineBox.y1 < lineBox.y0) return null;
    const linePixels = collect(lineBox);
    if (linePixels.length === 0) return null;

    const clampedChars = [];
    if (Array.isArray(charRects)) {
        for (const cr of charRects) {
            if (!cr || !(cr.w > 0) || !(cr.h > 0)) continue;
            const b = clampRect(cr);
            if (b.x1 < b.x0 || b.y1 < b.y0) continue;
            clampedChars.push(b);
        }
    }
    let inkPixels = linePixels;
    if (clampedChars.length > 0) {
        inkPixels = [];
        clampedChars.forEach((b, ci) => {
            for (const p of collect(b)) {
                p.ci = ci;
                inkPixels.push(p);
            }
        });
        // 字符框全部越界/无效 → 回退整行采样
        if (inkPixels.length === 0) inkPixels = linePixels;
    }
    const charConstrained = inkPixels !== linePixels;

    // Pass 2 背景基准:字符框收紧时,用"框外像素"(字间隙/行内空白,保证是背景)
    // 聚出最多 INK_BG_CLUSTER_MAX 个背景色——行内可能同时有两种背景(如徽章内
    // 绿底 + 外围黄底,亮度接近),单一中位数基准会偏向占比大的一方,把少数背景
    // 误判成文字(绿底黑字嵌在黄底上 → 字色采成绿黑混合)。文字候选须与所有
    // 背景簇都足够远。框外像素过少(字符框铺满整行)退回整行中位数簇单基准。
    const thr2 = INK_DIST_THRESHOLD * INK_DIST_THRESHOLD;
    let refBgs = null;
    let refBg = null;
    if (charConstrained) {
        const boxKeys = new Set(inkPixels.map((p) => p.key));
        const bgPixels = linePixels.filter((p) => !boxKeys.has(p.key));
        if (bgPixels.length >= INK_BG_PIXEL_MIN) {
            const clusters = clusterBgColors(bgPixels, INK_DIST_THRESHOLD, INK_BG_CLUSTER_MAX);
            // 最大簇无条件保留为主背景:字符框铺满行时,主背景在框内(字间隙)
            // 天然多于框外,不能用内外占比否决——否则主背景被踢出背景模型,
            // 整行字色会采成背景色(画在原底上=隐形)。
            // 次要簇:框内出现多于框外 → 是文字色(同色装饰/火花) → 不作背景。
            refBgs = clusters.filter((c, i) => {
                if (i === 0) return true;
                let inCount = 0;
                for (const p of inkPixels) {
                    if ((p.r - c.r) ** 2 + (p.g - c.g) ** 2 + (p.b - c.b) ** 2 <= thr2) inCount++;
                }
                return inCount < c.n;
            });
            if (refBgs.length === 0) refBgs = null;
        }
    }
    if (!refBgs) {
        refBg = medianClusterBg(linePixels);
    }
    if (!refBg && !refBgs) return null;

    // Pass 2 文字候选:只从候选池里找,与(所有)背景基准距离超阈值
    const candidates = [];
    for (const p of inkPixels) {
        let d2;
        if (refBgs) {
            d2 = Infinity;
            for (const c of refBgs) {
                const d = (p.r - c.r) ** 2 + (p.g - c.g) ** 2 + (p.b - c.b) ** 2;
                if (d < d2) d2 = d;
            }
        } else {
            d2 = (p.r - refBg.r) ** 2 + (p.g - refBg.g) ** 2 + (p.b - refBg.b) ** 2;
        }
        if (d2 > thr2) candidates.push({r: p.r, g: p.g, b: p.b, d2, ci: p.ci});
    }
    if (candidates.length < Math.max(INK_MIN_CANDIDATES, inkPixels.length * INK_MIN_CANDIDATE_RATIO)) {
        return null;
    }
    // 占比上限只约束"候选池=整行"的路径——字符框内墨水占比天然偏高,不适用
    if (!charConstrained && candidates.length > linePixels.length * INK_MAX_CANDIDATE_RATIO) {
        return null;
    }

    // 字形核心:距离最远的前 N%。核心内部再按颜色聚类，使用最大簇均值作为
    // 行级字色，并记录最大簇占比作为可信度。真实字形通常在核心区高度同色；
    // 壁纸/图标噪声往往会分散到多个颜色簇。
    candidates.sort((a, b) => b.d2 - a.d2);
    const coreN = Math.max(1, Math.ceil(candidates.length * INK_CORE_RATIO));
    const coreClusters = greedyColorClusters(candidates.slice(0, coreN), INK_DIST_THRESHOLD)
        .sort((a, b) => b.n - a.n);
    const dominantCore = coreClusters[0];
    const inkConfidence = dominantCore.n / coreN;
    const ink = {
        r: Math.round(dominantCore.sr / dominantCore.n),
        g: Math.round(dominantCore.sg / dominantCore.n),
        b: Math.round(dominantCore.sb / dominantCore.n),
    };

    // 对比度兜底:对实际绘制背景(无则对背景基准)校验可读性
    const checkBg = parseRgb(drawnBgCss || '') || refBg || (refBgs && refBgs[0]) || null;
    if (!checkBg || wcagContrastRatio(ink, checkBg) < INK_MIN_CONTRAST) {
        return null;
    }
    const inkCss = `rgb(${ink.r}, ${ink.g}, ${ink.b})`;

    // 逐字符分桶:字符框内候选独立求均值,可读则单独立色
    let charInks = null;
    if (charConstrained) {
        charInks = clampedChars.map(() => null);
        const buckets = clampedChars.map(() => []);
        for (const c of candidates) {
            if (c.ci !== undefined && c.ci < buckets.length) buckets[c.ci].push(c);
        }
        buckets.forEach((bucket, i) => {
            if (bucket.length < INK_CHAR_MIN_CANDIDATES) return;
            // 桶内可能混入穿过字形的装饰线像素,取最大颜色簇的均值而非全桶均值,
            // 避免两色混合出中间色(橙字+绿线 → 橄榄绿)
            const top = greedyColorClusters(bucket, INK_DIST_THRESHOLD)
                .sort((a, b) => b.n - a.n)[0];
            const col = {
                r: Math.round(top.sr / top.n),
                g: Math.round(top.sg / top.n),
                b: Math.round(top.sb / top.n),
            };
            if (wcagContrastRatio(col, checkBg) < INK_MIN_CONTRAST) return;
            charInks[i] = `rgb(${col.r}, ${col.g}, ${col.b})`;
        });
        // 归一化:相近色聚到同一簇均值,消除逐字采样噪声的同色微差波动
        charInks = quantizeInkColors(charInks, INK_DIST_THRESHOLD);
    }

    return {ink: inkCss, charInks, inkConfidence};
}

/** 从当前裁剪区像素采样原文字色(sampleOriginalInkColorFromPixels 的模块态入口)。 */
function sampleOriginalInkColor(rect, drawnBgCss, charRects) {
    return sampleOriginalInkColorFromPixels(cropImageData, rect, drawnBgCss, charRects);
}

/** 模块态完整版:行级 + 逐字符字色(嵌图分段着色用)。 */
function sampleOriginalInkColors(rect, drawnBgCss, charRects) {
    return sampleOriginalInkColorsFromPixels(cropImageData, rect, drawnBgCss, charRects);
}

/** RGB 欧氏距离平方;任一端解析失败返回 Infinity(视为不同色)。 */
function rgbDist2(a, b) {
    if (!a || !b) return Infinity;
    return (a.r - b.r) ** 2 + (a.g - b.g) ** 2 + (a.b - b.b) ** 2;
}

/**
 * 逐字符颜色归一化:相近色(距离 ≤ tol)贪心聚到同一簇,全部替换为簇均值,
 * 消除逐字采样噪声导致的"同色微差"波动(相邻红字呈现不同红)。
 * null 位(继承整行色)原样保留。
 */
export function quantizeInkColors(charInks, tol = 60) {
    const tol2 = tol * tol;
    const clusters = [];   // {sr, sg, sb, n}
    const assign = charInks.map((c) => {
        const rgb = parseRgb(c || '');
        if (!rgb) return -1;
        for (let i = 0; i < clusters.length; i++) {
            const cl = clusters[i];
            if (rgbDist2(rgb, {r: cl.sr / cl.n, g: cl.sg / cl.n, b: cl.sb / cl.n}) <= tol2) {
                cl.sr += rgb.r;
                cl.sg += rgb.g;
                cl.sb += rgb.b;
                cl.n++;
                return i;
            }
        }
        clusters.push({sr: rgb.r, sg: rgb.g, sb: rgb.b, n: 1});
        return clusters.length - 1;
    });
    const means = clusters.map((cl) => `rgb(${Math.round(cl.sr / cl.n)}, ${Math.round(cl.sg / cl.n)}, ${Math.round(cl.sb / cl.n)})`);
    return charInks.map((c, i) => (assign[i] >= 0 ? means[assign[i]] : null));
}

/**
 * 译文颜色保真门槛。沿用原文字色采样的 2.5 下限，避免误杀白底绿字、
 * 黑底蓝字等明显彩色文字；更低对比度仍回退背景对比色。
 */
const TRANSLATED_INK_MIN_CONTRAST = 2.5;
/** 彩色主色比中性色更容易来自壁纸噪声，因此要求更高的一致性与覆盖率。 */
const TRANSLATED_COLOR_DOMINANCE = 0.75;
const TRANSLATED_COLOR_COVERAGE = 0.6;
const TRANSLATED_NEUTRAL_DOMINANCE = 0.6;
const TRANSLATED_NEUTRAL_COVERAGE = 0.45;
/** 无逐字符框时，整行核心颜色簇至少达到此占比才接受彩色。 */
const TRANSLATED_LINE_COLOR_CONFIDENCE = 0.8;

function isNeutralRgb(rgb) {
    return !!rgb && Math.max(rgb.r, rgb.g, rgb.b) - Math.min(rgb.r, rgb.g, rgb.b) <= 24;
}

/**
 * 从原文字色采样结果中选择译文整行色。
 *
 * 规则：
 * - 不把逐字符色段按长度映射到译文；只选一个有足够字符共识的主色。
 * - 黑/白/灰中性色允许较宽松的共识；彩色需覆盖至少 60% 字符且占有效
 *   采样的 75%，避免桌面壁纸、图标和抗锯齿噪声被放大。
 * - 无字符框时，彩色行需通过核心颜色簇可信度校验。
 * - 对可解析的实际绘制背景执行 2.5:1 保真门槛。
 */
export function chooseStableTranslatedInk(sampled, fallbackColor, drawnBgCss = null) {
    const fallback = fallbackColor || '#111';
    if (!sampled || !sampled.ink) return fallback;

    const bgRgb = parseRgb(drawnBgCss || '');
    const readable = (css) => {
        const rgb = parseRgb(css || '');
        return !!rgb && (!bgRgb || wcagContrastRatio(rgb, bgRgb) >= TRANSLATED_INK_MIN_CONTRAST);
    };
    const sampledRgb = parseRgb(sampled.ink);
    const charInks = Array.isArray(sampled.charInks) ? sampled.charInks : null;
    if (!charInks || charInks.length === 0) {
        const stableLineColor = Number.isFinite(sampled.inkConfidence)
            && sampled.inkConfidence >= TRANSLATED_LINE_COLOR_CONFIDENCE;
        return (isNeutralRgb(sampledRgb) || stableLineColor) && readable(sampled.ink)
            ? sampled.ink : fallback;
    }

    const normalized = quantizeInkColors(charInks, INK_DIST_THRESHOLD);
    const counts = new Map();
    let validCount = 0;
    for (const css of normalized) {
        if (!css) continue;
        validCount++;
        counts.set(css, (counts.get(css) || 0) + 1);
    }
    if (validCount === 0) {
        return isNeutralRgb(sampledRgb) && readable(sampled.ink) ? sampled.ink : fallback;
    }

    let dominantColor = null;
    let dominantCount = 0;
    for (const [css, count] of counts) {
        if (count > dominantCount) {
            dominantColor = css;
            dominantCount = count;
        }
    }
    const dominantRgb = parseRgb(dominantColor || '');
    const dominance = dominantCount / validCount;
    const coverage = validCount / charInks.length;
    const neutral = isNeutralRgb(dominantRgb);
    const singleCharStrong = charInks.length === 1 && validCount === 1
        && Number.isFinite(sampled.inkConfidence)
        && sampled.inkConfidence >= TRANSLATED_LINE_COLOR_CONFIDENCE;
    const enoughSupport = neutral
        ? dominance >= TRANSLATED_NEUTRAL_DOMINANCE && coverage >= TRANSLATED_NEUTRAL_COVERAGE
        : singleCharStrong || (validCount >= 2
            && dominance >= TRANSLATED_COLOR_DOMINANCE
            && coverage >= TRANSLATED_COLOR_COVERAGE);
    if (enoughSupport && readable(dominantColor)) return dominantColor;

    // 行级结果若稳定落在中性色，可吸收逐字符框中的少量彩色离群点。
    return isNeutralRgb(sampledRgb) && readable(sampled.ink) ? sampled.ink : fallback;
}

// ── 译文模式代码行分段着色(0.23.x)─────────────────────────────

/**
 * 统计逐字符采样的"扎实结构簇"。
 *
 * 对归并后的颜色簇做三道过滤：占比 ≥ INK_CODE_CLUSTER_RATIO_MIN（碎片簇
 * 不计）、对背景 WCAG ≥ INK_MIN_CONTRAST（不可读色不计）、亮度 <
 * INK_CODE_CLUSTER_LUM_MAX（白色文字主体/浅彩抗锯齿边不是结构色——
 * corpus 实测真语法色蓝/紫/橙/黄绿的亮度均 < 170，而误判行的扎实簇全是
 * 近白/浅彩：白字主体 #e0e2e3、图标浅彩边 #99e3fe）。
 *
 * @param {Array<string|null>} charInks 逐字符颜色（null 不计入占比分母）
 * @param {{r: number, g: number, b: number}} bgRgb 实际绘制背景
 * @returns {Array<{rgb: {r: number, g: number, b: number}, ratio: number}>}
 */
export function countSolidInkClusters(charInks, bgRgb) {
    if (!Array.isArray(charInks) || charInks.length === 0 || !bgRgb) return [];
    const validTotal = charInks.filter(Boolean).length;
    if (validTotal === 0) return [];
    const normalized = quantizeInkColors(charInks, INK_DIST_THRESHOLD);
    const counts = new Map();
    for (const css of normalized) {
        if (!css) continue;
        counts.set(css, (counts.get(css) || 0) + 1);
    }
    const solid = [];
    for (const [css, n] of counts) {
        const rgb = parseRgb(css);
        if (!rgb) continue;
        const ratio = n / validTotal;
        if (ratio < INK_CODE_CLUSTER_RATIO_MIN) continue;
        if (wcagContrastRatio(rgb, bgRgb) < INK_MIN_CONTRAST) continue;
        const lum = 0.2126 * rgb.r + 0.7152 * rgb.g + 0.0722 * rgb.b;
        if (lum >= INK_CODE_CLUSTER_LUM_MAX) continue;
        solid.push({rgb, ratio});
    }
    return solid;
}

/**
 * 判定是否"代码特征行"（译文模式是否启用分段着色）。
 *
 * 主判据：charInks 覆盖率 ≥ INK_CODE_COVERAGE_MIN（文字密集行）且扎实结构
 * 簇数 ∈ [INK_CODE_CLUSTERS_MIN, INK_CODE_CLUSTERS_MAX]——下限排除整行
 * 单色（无分段意义，走整行快路径），上限排除图标彩边/照片纹理的杂色花斑。
 * corpus 验证：coding 正文 0 漏检；discord / x com / 桌面图标的白底彩边
 * 误判全部排除；commit 文件状态色、google 关键词高亮等真结构色保留。
 *
 * @param {{charInks: Array<string|null>|null}} sampled sampleOriginalInkColors 的结果
 * @param {string} drawnBgCss 实际绘制背景（结构簇对比度的校验基准）
 * @returns {boolean}
 */
export function isCodeLikeInkLayout(sampled, drawnBgCss) {
    if (!sampled || !Array.isArray(sampled.charInks) || sampled.charInks.length === 0) return false;
    const bgRgb = parseRgb(drawnBgCss || '');
    if (!bgRgb) return false;
    const valid = sampled.charInks.filter(Boolean).length;
    if (valid / sampled.charInks.length < INK_CODE_COVERAGE_MIN) return false;
    const solid = countSolidInkClusters(sampled.charInks, bgRgb);
    return solid.length >= INK_CODE_CLUSTERS_MIN && solid.length <= INK_CODE_CLUSTERS_MAX;
}

/**
 * 合并分段结果中的碎片段。
 *
 * 颜色距离区分不了"采样噪声变体"和"真语法色"（corpus 实测两者分布重叠：
 * 注释行噪声变体簇间距离 ≈ 98，语法色区分距离 ≈ 110），但 run 持续性可以
 * ——噪声是 1-2 字符的孤立短段，语法色是连续长段。占比 <
 * INK_FRAGMENT_RATIO_MAX 的短段并入色距 ≤ INK_FRAGMENT_MERGE_DIST 的相邻
 * 长段；与两邻段都超过色距的短段保留（可能是真语法色，如行尾独立彩色标点）。
 *
 * @param {Array<{text: string, color: string}>} segments buildInkSegments 的输出
 * @returns {Array<{text: string, color: string}>|null} 合并后的分段
 */
export function mergeTranslatedInkFragments(segments) {
    if (!Array.isArray(segments) || segments.length === 0) return null;
    if (segments.length === 1) return segments;
    const segs = segments.map((s) => ({text: s.text, color: s.color, rgb: parseRgb(s.color)}));
    for (;;) {
        if (segs.length <= 1) break;
        const totalLen = segs.reduce((n, s) => n + s.text.length, 0);
        let minIdx = -1;
        let minLen = Infinity;
        segs.forEach((s, i) => {
            if (s.text.length < minLen) {
                minLen = s.text.length;
                minIdx = i;
            }
        });
        if (segs[minIdx].text.length / totalLen >= INK_FRAGMENT_RATIO_MAX) break;
        const left = segs[minIdx - 1];
        const right = segs[minIdx + 1];
        const dL = left ? rgbDist2(segs[minIdx].rgb, left.rgb) : Infinity;
        const dR = right ? rgbDist2(segs[minIdx].rgb, right.rgb) : Infinity;
        if (Math.min(dL, dR) > INK_FRAGMENT_MERGE_DIST * INK_FRAGMENT_MERGE_DIST) break;
        if (dL <= dR) {
            left.text += segs[minIdx].text;
            segs.splice(minIdx, 1);
        } else {
            right.text = segs[minIdx].text + right.text;
            segs.splice(minIdx, 1);
        }
    }
    return segs.map(({text, color}) => ({text, color}));
}

/**
 * 译文模式分段着色的组合入口：null 填充 → 色系归并 → 比例映射 → 碎片合并。
 *
 * null 位必须先填充：采样失败字符回退的行级色与相邻实采色"相近但不相等"，
 * run 合并的 30 容差合不掉，是分段碎片化的主要来源。归并容差沿用
 * INK_DIST_THRESHOLD 不放大——90 会把黄绿关键字和灰青标识符错误合并
 * （两者距离 ≈ 110，与噪声变体距离 ≈ 98 重叠）。
 */
function buildTranslatedInkSegments(line, displayText, fallbackColor) {
    const sampled = line.inkSampled;
    if (!sampled || !Array.isArray(sampled.charInks) || sampled.charInks.length === 0) return null;
    const filled = sampled.charInks.map((c) => c || sampled.ink);
    const wordAligned = alignInkColorsToWords(line.srcText || '', filled);
    const premerged = quantizeInkColors(wordAligned, INK_DIST_THRESHOLD);
    const raw = buildInkSegments((line.srcText || '').length, displayText, premerged, fallbackColor);
    return raw ? mergeTranslatedInkFragments(raw) : null;
}

/**
 * 把逐字符颜色按"词单元"对齐：同一词内的字符统一为词内主色。
 *
 * 词单元切分（正则级，无语义分词）：连续字母/数字/下划线为一个词
 * （camelCase / snake_case 不拆——一个词一个色正是目标观感）；单个非空白
 * 字符（标点/符号）自成单元；CJK 逐字成单元（相邻同色由 run 合并收拢）。
 * 词内主色 = 词内非 null 颜色量化（INK_DIST_THRESHOLD）后的最大簇均值。
 *
 * 动机：逐字符采样噪声让同一词内出现颜色抖动（"chatPrompt" 中段发灰、
 * 译文分段在词中间断开），以词为染色单位后 run 边界天然落在词边界。
 *
 * @param {string} srcText 原文（切词基准）
 * @param {Array<string|null>} charInks 逐字符颜色
 * @returns {Array<string|null>} 词对齐后的逐字符颜色（全 null 词保留 null）
 */
export function alignInkColorsToWords(srcText, charInks) {
    if (!srcText || !Array.isArray(charInks) || charInks.length === 0) return null;
    const out = charInks.slice();
    const n = Math.min(srcText.length, charInks.length);
    const isWordChar = (ch) => /[\w]/.test(ch);
    let i = 0;
    while (i < n) {
        if (/\s/.test(srcText[i])) {
            // 空白不可见，颜色跟随前一个字符，避免把同色词的 run 无谓断开
            out[i] = i > 0 ? out[i - 1] : out[i];
            i++;
            continue;
        }
        let j = i + 1;
        if (isWordChar(srcText[i])) {
            while (j < n && isWordChar(srcText[j])) j++;
        }
        // [i, j) 为一个染色单元（词或单标点）
        const colors = [];
        for (let k = i; k < j; k++) {
            if (out[k]) colors.push(out[k]);
        }
        if (colors.length > 0) {
            const quantized = quantizeInkColors(colors, INK_DIST_THRESHOLD);
            const counts = new Map();
            for (const css of quantized) {
                counts.set(css, (counts.get(css) || 0) + 1);
            }
            let best = null;
            let bestN = 0;
            for (const [css, cnt] of counts) {
                if (cnt > bestN) {
                    best = css;
                    bestN = cnt;
                }
            }
            for (let k = i; k < j; k++) out[k] = best;
        }
        i = j;
    }
    return out;
}

/** 相邻字色合并阈值:RGB 欧氏距离低于此值视为同色(采样噪声容差)。 */
const INK_SEGMENT_MERGE_DIST = 30;

/**
 * 把逐字符字色映射为显示文本的颜色分段（当前生产路径仅供原文模式使用）。
 *
 * 当显示文本长度与原文不同，按长度比例映射：原文 run [a, b) →
 * 显示文本 [a/srcLen·dstLen, b/srcLen·dstLen)。译文模式不调用本函数，避免
 * 无语义依据的跨语言颜色映射。相邻字符颜色相近(采样噪声内)
 * 先合并为同色 run;全行同色返回 null(调用方走整行 fillText 快路径)。
 * 分段首尾强制对齐 [0, dstLen],保证覆盖无缺口。
 *
 * @param {number} srcLen - 原文字符数(分段映射的基准长度)
 * @param {string} displayText - 实际要绘制的文本(译文或截断后的显示文本)
 * @param {Array<string|null>|null} charInks - 逐字符颜色(null = 继承整行字色)
 * @param {string} fallbackColor - 整行字色
 * @returns {Array<{text: string, color: string}>|null} null = 无需分段
 */
export function buildInkSegments(srcLen, displayText, charInks, fallbackColor) {
    if (!displayText || !srcLen || !Array.isArray(charInks) || charInks.length === 0
        || !fallbackColor) {
        return null;
    }
    const colorOf = (i) => charInks[i] || fallbackColor;

    // 1. 相邻同色合并为 run(锚点取 run 首字符颜色,防采样噪声漂移)
    const runs = [];
    let runStart = 0;
    let runColor = parseRgb(colorOf(0));
    for (let i = 1; i < charInks.length; i++) {
        const c = parseRgb(colorOf(i));
        if (rgbDist2(c, runColor) > INK_SEGMENT_MERGE_DIST * INK_SEGMENT_MERGE_DIST) {
            runs.push({
                start: runStart,
                end: i,
                color: runColor
                    ? `rgb(${runColor.r}, ${runColor.g}, ${runColor.b})` : fallbackColor,
            });
            runStart = i;
            runColor = c;
        }
    }
    runs.push({
        start: runStart,
        end: charInks.length,
        color: runColor
            ? `rgb(${runColor.r}, ${runColor.g}, ${runColor.b})` : fallbackColor,
    });
    if (runs.length <= 1) return null;

    // 2. 原文 run → 译文区间(比例映射,首尾钳位,顺序推进保证无缺口)
    const dstLen = displayText.length;
    const segments = [];
    let prevEnd = 0;
    for (let i = 0; i < runs.length; i++) {
        const run = runs[i];
        const end = i === runs.length - 1
            ? dstLen
            : Math.min(dstLen, Math.max(prevEnd, Math.round((run.end / srcLen) * dstLen)));
        if (end > prevEnd) {
            segments.push({text: displayText.slice(prevEnd, end), color: run.color});
            prevEnd = end;
        }
    }
    return segments.length > 0 ? segments : null;
}

/**
 * 迭代找到能塞进 rect.w * 0.95 的最大字号。
 *
 * 起始 rect.h * fontScale,每次 -1 逐步尝试(rect 高度一般不超过 60px,循环上限 ~50 次可控)。
 * 到达下限 8px 仍超宽 → 用 8px + 二分找最长前缀 + 省略号截断。
 * rect 极窄(连一个字都放不下 8px)→ 返回 size=0 让 drawOverlay 跳过。
 * 传入的 rect 通常是 fontRect(= 原 rect 换上折减后的 fontH,见 drawOverlay)。
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
        return {size, display: text};
    }
    // 到 minSize 仍超宽 → 截断 + 省略号
    const ellipsis = '…';
    const ellW = ctx.measureText(ellipsis).width;
    if (ellW > maxWidth) {
        ctx.restore();
        return {size: 0, display: ''};   // rect 极窄,一个省略号都放不下
    }
    let lo = 0, hi = text.length;
    while (lo < hi) {
        const mid = Math.floor((lo + hi + 1) / 2);
        if (ctx.measureText(text.slice(0, mid)).width + ellW <= maxWidth) lo = mid;
        else hi = mid - 1;
    }
    ctx.restore();
    return {size: minSize, display: text.slice(0, lo) + ellipsis};
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
