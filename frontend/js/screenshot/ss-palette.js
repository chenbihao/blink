//! 0.20.7：配色分析 UI 收敛——后端单一真源。
//!
//! 算法核心已迁移至 Rust `src/domain/palette.rs`，前端只负责：
//! - 调用 `analyze_palette` Tauri 命令（截图选区或图片编辑会话）
//! - 渲染角色色、推荐方案、生成方案
//! - 复制/格式化输出
//! - 显式基准色的配色方案生成（调 `generate_palette_schemes` 后端命令）
//!
//! **零数据回传**：截图来源只传物理坐标，后端从 SESSION 直接裁剪；
//! 编辑器来源传 `"editor"` + 选区 bitmap 坐标（crop），后端从编辑会话 SESSION 取 PNG → 按坐标裁剪。

import * as annot from './annotation-engine.js';
import {ss} from './ss-state.js';
import {getColorFormat, syncFromAnnot} from './ss-color-picker.js';
import {copyToClipboard as copyTextToClipboard, invoke} from '../shared/api.js';
import {cssRectToBitmap} from './ss-selection-geometry.js';
import {IMAGE_SOURCE} from './image-editor-session.js';
import {formatOutput, formatPaletteColors, rgbToHex} from './palette-format.js';

// ── 纯工具函数（已提取至 palette-format.js，此处仅保留 import）──────────────
//
// P1-1：删除前端 HSL 双算法（rgbToHsl / hslToRgb / generateDesignPalettes）。
// 所有色彩运算统一走后端 Rust `src/domain/palette.rs` OKLCH 单一真源。
// 前端"生成当前色配色方案"展开时调 `generate_palette_schemes` Tauri 命令。
//
// P4-1：纯格式化函数（rgbToHex / hexToHslString / formatOutput / formatAsCssVariables /
// formatPaletteColors）已提取至 `palette-format.js`，ss-palette.js 和测试共同 import 同一模块。

// ── DOM 引用 ──────────────────────────────────────────────

let extractBtn = null;
let paletteEl = null;
let harmonyEl = null;
let harmonySwatches = null;
let moreSchemesEl = null;
let copyAllBtn = null;
let copyStatusEl = null;
let themeSummaryEl = null;
let moreToggleEl = null;
let actionsRowEl = null;
let selectionHintEl = null; // P1-3：操作行左侧提示/计数
let copyMenuEl = null; // P1-3：复制模式下拉菜单

/** 防抖定时器 */
let debounceTimer = 0;

/** 新截图/图片编辑会话开始时清理上一轮分析、展开与多选状态。 */
export function resetPaletteState() {
    clearTimeout(debounceTimer);
    debounceTimer = 0;
    ss.paletteEpoch++;
    ss.paletteResult = null;
    ss.paletteSelected = new Set();
    ss.paletteColorOrder = [];
    // P1-3：paletteFormat 已移除，格式由顶部 .color-format 统一管理
    ss.paletteMoreExpanded = false;
    ss.paletteAnchorHex = null;

    if (paletteEl) {
        paletteEl.replaceChildren();
        paletteEl.hidden = true;
    }
    if (harmonySwatches) harmonySwatches.replaceChildren();
    if (moreSchemesEl) {
        moreSchemesEl.replaceChildren();
        moreSchemesEl.hidden = true;
    }
    if (harmonyEl) harmonyEl.hidden = true;
    if (themeSummaryEl) {
        themeSummaryEl.textContent = '';
        themeSummaryEl.title = '';
        themeSummaryEl.hidden = true;
    }
    if (actionsRowEl) actionsRowEl.hidden = true;
    if (copyStatusEl) copyStatusEl.textContent = '';
    if (extractBtn) extractBtn.disabled = false;
    if (moreToggleEl) moreToggleEl.textContent = '生成当前色配色方案';
    closeCopyMenu();
    updateCopyButtonLabel();
}

// ── 配色分析触发 ───────────────────────────────────────────

/**
 * 触发配色分析（防抖 120ms）。
 * 选区变化后 120ms 启动最新分析，旧 epoch 不覆盖新结果。
 *
 * **零数据回传**：
 * - 截图来源：前端只传物理坐标 (bitmap 坐标系 = SESSION 坐标系)，后端从 SESSION 裁剪 BGRA → swap → 分析
 * - 编辑器来源：前端传 `"editor"` + 选区 bitmap 坐标 (crop)，后端从编辑会话 SESSION 取 PNG → 解码 → 按坐标裁剪 → 分析
 */
export function triggerPaletteAnalysis() {
    // P1-4：长截图来源禁用配色提取
    if (ss.editorSession.source === IMAGE_SOURCE.LONG_SCREENSHOT) {
        if (copyStatusEl) copyStatusEl.textContent = '长截图不支持配色提取';
        return;
    }
    clearTimeout(debounceTimer);
    const DEBOUNCE_MS = 120;
    debounceTimer = setTimeout(async () => {
        ss.paletteEpoch++;
        const epoch = ss.paletteEpoch;

        if (extractBtn) extractBtn.disabled = true;
        if (copyStatusEl) copyStatusEl.textContent = '正在分析选区配色…';

        try {
            const result = await invokeAnalyzePalette();
            if (epoch !== ss.paletteEpoch) return; // 旧 epoch 丢弃
            if (extractBtn) extractBtn.disabled = false;
            ss.paletteResult = result;
            renderPalette(result);
        } catch (err) {
            if (epoch !== ss.paletteEpoch) return;
            if (extractBtn) extractBtn.disabled = false;
            console.warn('[palette] 分析失败:', err);
            if (copyStatusEl) copyStatusEl.textContent = '配色分析失败';
            // 渲染空结果
            renderPalette({roles: [], recommended: [], full: [], empty: true});
        }
    }, DEBOUNCE_MS);
}

/**
 * 调用后端 `analyze_palette` 命令。
 * 根据当前会话来源选择参数：
 * - 截图来源（SCREENSHOT 且无 canvas 底图）：传物理坐标
 * - 编辑器来源（clipboard/history/pin/long-screenshot）：传 "editor"
 */
async function invokeAnalyzePalette() {
    const isScreenshotSource = ss.editorSession.source === IMAGE_SOURCE.SCREENSHOT
        && !ss.editorSession.canvasBacked;

    if (isScreenshotSource) {
        // 截图来源：从 CSS 选区转 bitmap 坐标（= SESSION 物理坐标）
        if (!ss.selCss) throw new Error('无选区');
        const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
        const bmp = cssRectToBitmap(ss.selCss, meta);
        return invoke('analyze_palette', {
            source: 'screenshot',
            x: bmp.x,
            y: bmp.y,
            w: bmp.w,
            h: bmp.h,
        });
    } else {
        // 编辑器来源：后端从编辑会话 SESSION 取 PNG，前端传选区 bitmap 坐标用于裁剪
        const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
        const crop = ss.selCss ? cssRectToBitmap(ss.selCss, meta) : null;
        return invoke('analyze_palette', {
            source: 'editor',
            crop: crop ? {x: crop.x, y: crop.y, w: crop.w, h: crop.h} : null,
        });
    }
}

// ── UI 渲染 ──────────────────────────────────────────────────

/**
 * 渲染配色结果到 UI。
 *
 * 后端返回的 JSON 字段为 snake_case（Rust serde 默认）：
 * - roles[i].rgb = [r, g, b], roles[i].hex, roles[i].role, roles[i].ratio
 * - recommended[i].colors = [[r,g,b], ...], .label, .scheme, .description
 * - sample.valid_pixels, sample.scanned_pixels, .width, .height, .mode
 * - theme.summary
 *
 * @param {{roles: Array, recommended: Array, full: Array, empty: boolean}} result
 */
function renderPalette(result) {
    if (!result || result.empty) {
        if (paletteEl) {
            paletteEl.innerHTML = '';
            paletteEl.hidden = true;
        }
        if (harmonyEl) harmonyEl.hidden = true;
        if (themeSummaryEl) {
            themeSummaryEl.textContent = '无有效像素';
            themeSummaryEl.hidden = false;
        }
        if (actionsRowEl) actionsRowEl.hidden = true;
        if (moreSchemesEl) moreSchemesEl.hidden = true;
        if (copyStatusEl) copyStatusEl.textContent = '';
        notifyPaletteLayoutChanged();
        return;
    }

    // 新一轮分析默认不多选；普通左键仍是"设为标注色"，Ctrl+左键才进入批量选择。
    ss.paletteSelected = new Set();
    ss.paletteColorOrder = collectPaletteColorOrder(result);
    ss.paletteMoreExpanded = false;
    const focusColor = result.recommended.find((scheme) => scheme.scheme === 'salient')?.colors?.[0];
    const firstColor = focusColor || result.roles[0]?.rgb;
    ss.paletteAnchorHex = firstColor ? rgbToHex(...firstColor) : null;
    if (copyStatusEl) copyStatusEl.textContent = '';
    updateGenerateButtonLabel();
    if (moreSchemesEl) moreSchemesEl.hidden = true;

    // 图片主题色作为推荐区第一张卡片，不暴露内部角色名。
    renderRoleSwatches(result.roles);
    paletteEl.hidden = false;
    if (themeSummaryEl) {
        const sample = result.sample;
        const sampleSize = sample?.width && sample?.height
            ? ` · 整图扫描 ${sample.width}×${sample.height}`
            : '';
        themeSummaryEl.textContent = `${result.theme?.summary || `${result.roles.length} 个主题色`}${sampleSize}`;
        themeSummaryEl.title = sample
            ? `逐像素扫描整块选区，共分析 ${sample.valid_pixels} 个有效像素；提取色均来自原图真实像素`
            : '';
        themeSummaryEl.hidden = false;
    }

    if (actionsRowEl) actionsRowEl.hidden = false;
    updateCopyButtonLabel();

    // 渲染推荐方案
    if (harmonyEl) {
        harmonyEl.hidden = false;
        renderRecommendedSchemes(result.recommended.filter((scheme) => scheme.scheme !== 'source'));
    }
    notifyPaletteLayoutChanged();
}

function updateCopyButtonLabel() {
    const count = ss.paletteSelected.size;
    // P1-3：操作行左侧——无选择显示"Ctrl + 单击可多选"，有选择显示数量
    if (selectionHintEl) {
        selectionHintEl.textContent = count > 0 ? `已选 ${count} 色` : 'Ctrl + 单击可多选';
    }
    if (copyAllBtn) {
        const label = copyAllBtn.querySelector('span');
        if (label) label.textContent = count > 0 ? `复制所选` : '复制所选';
        copyAllBtn.disabled = count === 0;
    }
}

function notifyPaletteLayoutChanged() {
    const dropdown = document.getElementById('color-dropdown');
    requestAnimationFrame(() => dropdown?.dispatchEvent(new CustomEvent('palette-layout-changed')));
}

function collectPaletteColorOrder(result) {
    const seen = new Set();
    const ordered = [];
    const addRgb = (rgb) => {
        const hex = rgbToHex(rgb[0], rgb[1], rgb[2]);
        if (seen.has(hex)) return;
        seen.add(hex);
        ordered.push(hex);
    };
    result.roles.forEach((role) => addRgb(role.rgb));
    result.recommended.forEach((scheme) => scheme.colors.forEach(addRgb));
    result.full.forEach((scheme) => scheme.colors.forEach(addRgb));
    return ordered;
}

function replacePaletteColorOrder(generatedSchemes = []) {
    if (!ss.paletteResult) return;
    const ordered = collectPaletteColorOrder(ss.paletteResult);
    const seen = new Set(ordered);
    for (const scheme of generatedSchemes) {
        for (const rgb of scheme.colors) {
            const hex = rgbToHex(...rgb);
            if (seen.has(hex)) continue;
            seen.add(hex);
            ordered.push(hex);
        }
    }
    ss.paletteColorOrder = ordered;
    ss.paletteSelected = new Set([...ss.paletteSelected].filter((hex) => seen.has(hex)));
    updateCopyButtonLabel();
}

function setAnnotationColor(hex) {
    annot.setColor(hex);
    ss.paletteAnchorHex = hex;
    const dot = document.getElementById('color-trigger-dot');
    if (dot) dot.style.background = hex;
    syncFromAnnot();
    updateGenerateButtonLabel();
    if (ss.paletteMoreExpanded) void renderGeneratedSchemes();
}

function updateGenerateButtonLabel() {
    if (!moreToggleEl) return;
    if (ss.paletteMoreExpanded) {
        moreToggleEl.textContent = '收起配色方案';
        return;
    }
    moreToggleEl.textContent = ss.paletteAnchorHex
        ? `生成 ${ss.paletteAnchorHex} 配色方案`
        : '生成当前色配色方案';
}

function togglePaletteSelection(hex) {
    const selected = !ss.paletteSelected.has(hex);
    if (selected) ss.paletteSelected.add(hex);
    else ss.paletteSelected.delete(hex);
    document.querySelectorAll(`[data-palette-hex="${hex}"]`)
        .forEach((el) => el.classList.toggle('is-selected', selected));
    updateCopyButtonLabel();
}

function bindPaletteColor(el, hex) {
    el.dataset.paletteHex = hex;
    el.classList.toggle('is-selected', ss.paletteSelected.has(hex));
    el.title = `${hex} · 左键设色 · Ctrl+左键多选 · 右键复制`;
    el.addEventListener('click', (e) => {
        e.stopPropagation();
        if (e.ctrlKey || e.metaKey) togglePaletteSelection(hex);
        else setAnnotationColor(hex);
    });
    el.addEventListener('contextmenu', (e) => {
        e.preventDefault();
        e.stopPropagation();
        void copyToClipboard(hex, el);
    });
    el.addEventListener('mousedown', (e) => e.stopPropagation());
}

/**
 * P1-3：创建带下拉菜单的"复制整组"按钮。
 * 左键直接按当前格式复制；右键或小箭头展开 list/css 模板菜单。
 */
function createCopyGroupBtn(scheme) {
    const wrap = document.createElement('div');
    wrap.className = 'dropdown-wrap palette-group-copy-wrap';

    const btn = document.createElement('button');
    btn.className = 'harmony-copy-group';
    btn.textContent = '复制整组';
    btn.addEventListener('click', (e) => {
        e.stopPropagation();
        closeAllGroupMenus();
        void copyScheme(scheme, btn);
    });
    btn.addEventListener('contextmenu', (e) => {
        e.preventDefault();
        e.stopPropagation();
        closeAllGroupMenus();
        menuEl.dataset.open = 'true';
    });
    btn.addEventListener('mousedown', (e) => e.stopPropagation());

    const menuEl = document.createElement('div');
    menuEl.className = 'dropdown palette-group-copy-menu';
    menuEl.dataset.open = 'false';
    const modes = [
        {mode: 'auto', label: '按当前格式'},
        {mode: 'list', label: '每行一个'},
        {mode: 'css', label: 'CSS 变量'},
    ];
    for (const m of modes) {
        const item = document.createElement('button');
        item.className = 'dropdown-item';
        item.innerHTML = `<span class="item-label">${m.label}</span>`;
        item.addEventListener('mousedown', (e) => e.stopPropagation());
        item.addEventListener('click', (e) => {
            e.stopPropagation();
            menuEl.dataset.open = 'false';
            void copyScheme(scheme, btn, m.mode);
        });
        menuEl.appendChild(item);
    }

    wrap.append(btn, menuEl);
    return wrap;
}

/** P1-3：关闭所有已展开的"复制整组"下拉菜单。 */
function closeAllGroupMenus() {
    document.querySelectorAll('.palette-group-copy-menu[data-open="true"]').forEach((el) => {
        el.dataset.open = 'false';
    });
}

/**
 * 渲染角色色 swatch。
 * 每个色块：
 * - 左键 → 设为当前色 + 同步色盘
 * - 右键 → 复制 HEX
 * - Ctrl+左键 → 加入/移出跨方案多选
 *
 * @param {Array} roles - 角色色数组
 */
function renderRoleSwatches(roles) {
    if (!paletteEl) return;
    paletteEl.innerHTML = '';

    const header = document.createElement('div');
    header.className = 'harmony-scheme-header';
    const heading = document.createElement('span');
    heading.className = 'harmony-scheme-label';
    heading.textContent = '图片主题色';
    const description = document.createElement('span');
    description.className = 'harmony-scheme-description';
    description.textContent = `原图聚类 · ${roles.length} 色`;
    const copyGroupWrap = createCopyGroupBtn({label: '图片主题色', colors: roles.map((role) => role.rgb)});
    header.append(heading, description, copyGroupWrap);

    const row = document.createElement('div');
    row.className = 'palette-theme-row';

    for (const role of roles) {
        const hex = role.hex;
        const item = document.createElement('button');
        item.className = 'palette-theme-color';
        item.dataset.role = role.role;

        const colorBlock = document.createElement('span');
        colorBlock.className = 'palette-theme-swatch';
        colorBlock.style.background = hex;

        const text = document.createElement('span');
        text.className = 'palette-theme-label';
        text.textContent = `${(role.ratio * 100).toFixed(0)}%`;

        item.append(colorBlock, text);
        bindPaletteColor(item, hex);
        row.appendChild(item);
    }
    paletteEl.append(header, row);
}

/**
 * 渲染推荐方案（首屏最多 3 个）。
 * @param {Array} schemes
 */
function renderRecommendedSchemes(schemes) {
    if (!harmonySwatches) return;
    harmonySwatches.innerHTML = '';

    // 推荐方案不再使用 tab 二次点击，最多 3 组直接完整展开。
    for (const scheme of schemes.slice(0, 3)) {
        renderSingleHarmony(scheme);
    }
}

/**
 * 渲染单个 harmony 方案。
 * @param {{label: string, scheme: string, colors: number[][], source_kind?: string, confidence?: number, description?: string}} scheme
 */
function renderSingleHarmony(scheme, target = harmonySwatches) {
    if (!target) return;

    const card = document.createElement('section');
    card.className = 'harmony-scheme-card';

    // P1-2：降级状态标记——confidence < 1.0 时显示降级指示
    const isDegraded = typeof scheme.confidence === 'number' && scheme.confidence < 1.0;
    if (isDegraded) {
        card.classList.add('is-degraded');
    }

    const header = document.createElement('div');
    header.className = 'harmony-scheme-header';
    const label = document.createElement('span');
    label.className = 'harmony-scheme-label';
    label.textContent = scheme.label;
    const description = document.createElement('span');
    description.className = 'harmony-scheme-description';
    description.textContent = scheme.description || '';
    const copyGroupWrap = createCopyGroupBtn(scheme);
    header.append(label, description, copyGroupWrap);
    card.appendChild(header);

    // 降级提示条
    if (isDegraded) {
        const notice = document.createElement('div');
        notice.className = 'harmony-degraded-notice';
        notice.textContent = '⚠ 未找到满足 WCAG 可读性约束的组合，已降级展示原图色';
        card.appendChild(notice);
    }

    // 色块行
    const row = document.createElement('div');
    row.className = 'harmony-color-row';
    for (const rgb of scheme.colors) {
        const hex = rgbToHex(rgb[0], rgb[1], rgb[2]);
        const swatch = document.createElement('button');
        swatch.className = 'palette-swatch';
        swatch.style.background = hex;
        bindPaletteColor(swatch, hex);
        row.appendChild(swatch);
    }
    card.appendChild(row);

    target.appendChild(card);
}

/**
 * 渲染完整方案（"更多方案"展开时）。
 * @param {Array} schemes
 */
function renderFullSchemes(schemes, heading = '') {
    if (!moreSchemesEl) return;
    moreSchemesEl.innerHTML = '';
    if (heading) {
        const label = document.createElement('div');
        label.className = 'palette-generated-heading';
        label.textContent = heading;
        moreSchemesEl.appendChild(label);
    }
    schemes.forEach((scheme) => renderSingleHarmony(scheme, moreSchemesEl));
}

async function renderGeneratedSchemes() {
    if (!ss.paletteAnchorHex || !ss.paletteResult) return;
    const sourceColors = ss.paletteResult.roles.map((role) => role.rgb);
    try {
        const schemes = await invoke('generate_palette_schemes', {
            anchorHex: ss.paletteAnchorHex,
            sourceColors,
        });
        renderFullSchemes(schemes, `基于 ${ss.paletteAnchorHex} 生成 · 非原图提取色`);
        replacePaletteColorOrder(schemes);
    } catch (err) {
        console.warn('[palette] 生成配色方案失败:', err);
        if (copyStatusEl) copyStatusEl.textContent = '生成配色方案失败';
    }
}

// ── 复制 ──────────────────────────────────────────────────

/**
 * 复制文本到剪贴板，并显示视觉反馈。
 * @param {string} text
 * @param {HTMLElement} [feedbackEl] - 显示 copied class 的元素
 */
async function copyToClipboard(text, feedbackEl) {
    try {
        await copyTextToClipboard(text);
        if (feedbackEl) {
            feedbackEl.classList.add('copied');
            setTimeout(() => feedbackEl.classList.remove('copied'), 600);
        }
        if (copyStatusEl) copyStatusEl.textContent = '已复制到剪贴板';
        return true;
    } catch (error) {
        console.warn('[palette] 写入剪贴板失败', error);
        if (feedbackEl) {
            feedbackEl.classList.add('copy-failed');
            setTimeout(() => feedbackEl.classList.remove('copy-failed'), 600);
        }
        if (copyStatusEl) copyStatusEl.textContent = '复制失败，请重试';
        return false;
    }
}

function formatPaletteColorsForUi(hexColors, mode) {
    return formatPaletteColors(hexColors, mode, getColorFormat, ss.paletteResult?.roles);
}

/** P1-3：复制整组方案色。支持模式下拉菜单选择 list/css 模板。 */
async function copyScheme(scheme, feedbackEl, mode) {
    const seen = new Set();
    const hexColors = [];
    for (const rgb of scheme.colors) {
        const hex = rgbToHex(rgb[0], rgb[1], rgb[2]);
        if (seen.has(hex)) continue;
        seen.add(hex);
        hexColors.push(hex);
    }
    const fmt = mode || 'auto';
    const text = fmt === 'list'
        ? formatOutput(hexColors, 'list')
        : formatPaletteColorsForUi(hexColors, fmt);
    const copied = await copyToClipboard(text, feedbackEl);
    if (copied && copyStatusEl) {
        const modeLabel = fmt === 'list' ? ' · 每行一个' : fmt === 'css' ? ' · CSS 变量' : '';
        copyStatusEl.textContent = `已复制"${scheme.label}"${hexColors.length} 色${modeLabel}`;
    }
}

/** P1-3：复制 Ctrl+左键选中的任意主题色/方案色。支持模式下拉菜单。 */
async function copySelected(mode) {
    const hexColors = ss.paletteColorOrder.filter((hex) => ss.paletteSelected.has(hex));
    if (hexColors.length === 0) return;

    const fmt = mode || 'auto';
    const text = fmt === 'list'
        ? formatOutput(hexColors, 'list')
        : formatPaletteColorsForUi(hexColors, fmt);
    const copied = await copyToClipboard(text, copyAllBtn);
    if (copied && copyStatusEl) {
        const modeLabel = fmt === 'list' ? ' · 每行一个' : fmt === 'css' ? ' · CSS 变量' : ` · ${getColorFormat().toUpperCase()}`;
        copyStatusEl.textContent = `已复制 ${hexColors.length} 色${modeLabel}`;
    }
}

// ── 初始化 ──────────────────────────────────────────────────

/**
 * 初始化配色提取模块（幂等，在 initColorPicker 中调用）。
 */
export function initPalette() {
    const dropdown = document.getElementById('color-dropdown');
    if (!dropdown) return;

    extractBtn = dropdown.querySelector('.palette-extract-btn');
    paletteEl = dropdown.querySelector('.palette-extracted');
    harmonyEl = dropdown.querySelector('.palette-harmony');
    harmonySwatches = harmonyEl ? harmonyEl.querySelector('.harmony-swatches') : null;
    moreSchemesEl = dropdown.querySelector('.palette-more-schemes');
    copyAllBtn = dropdown.querySelector('.palette-copy-all');
    copyStatusEl = dropdown.querySelector('.palette-copy-status');
    themeSummaryEl = dropdown.querySelector('.palette-theme-summary');
    moreToggleEl = dropdown.querySelector('.palette-more-toggle');
    actionsRowEl = dropdown.querySelector('.palette-actions-row');
    selectionHintEl = dropdown.querySelector('.palette-selection-hint');
    copyMenuEl = dropdown.querySelector('.palette-copy-menu');

    dropdown.addEventListener('wheel', (e) => e.stopPropagation(), {passive: true});

    if (extractBtn) {
        extractBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            triggerPaletteAnalysis();
        });
        extractBtn.addEventListener('mousedown', (e) => e.stopPropagation());
    }

    // P1-3：复制按钮——左键直接按当前格式复制，不展开菜单
    if (copyAllBtn) {
        copyAllBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            // 如果菜单已展开，先关闭再复制
            closeCopyMenu();
            void copySelected();
        });
        copyAllBtn.addEventListener('mousedown', (e) => e.stopPropagation());
    }

    // P1-3：复制模式下拉菜单
    if (copyMenuEl) {
        copyMenuEl.querySelectorAll('.dropdown-item').forEach((item) => {
            item.addEventListener('mousedown', (e) => e.stopPropagation());
            item.addEventListener('click', (e) => {
                e.stopPropagation();
                const mode = item.dataset.copyMode;
                closeCopyMenu();
                void copySelected(mode);
            });
        });
        // 点击外部关闭菜单
        document.addEventListener('mousedown', (e) => {
            if (copyMenuEl?.dataset.open === 'true' && !copyMenuEl.contains(e.target) && e.target !== copyAllBtn) {
                closeCopyMenu();
            }
            // P1-3：关闭所有"复制整组"下拉菜单（点击外部时）
            closeAllGroupMenus();
        });
    }

    // 显式基准色的配色生成器展开/折叠
    if (moreToggleEl) {
        moreToggleEl.addEventListener('click', (e) => {
            e.stopPropagation();
            ss.paletteMoreExpanded = !ss.paletteMoreExpanded;
            updateGenerateButtonLabel();
            if (ss.paletteMoreExpanded && ss.paletteAnchorHex && moreSchemesEl) {
                void renderGeneratedSchemes();
                moreSchemesEl.hidden = false;
            } else if (moreSchemesEl) {
                moreSchemesEl.hidden = true;
                replacePaletteColorOrder();
            }
            notifyPaletteLayoutChanged();
        });
        moreToggleEl.addEventListener('mousedown', (e) => e.stopPropagation());
    }
}

/** P1-3：打开/关闭复制模式下拉菜单。 */
function openCopyMenu() {
    if (copyMenuEl) copyMenuEl.dataset.open = 'true';
}

function closeCopyMenu() {
    if (copyMenuEl) copyMenuEl.dataset.open = 'false';
}

function toggleCopyMenu() {
    if (!copyMenuEl) return;
    copyMenuEl.dataset.open = copyMenuEl.dataset.open === 'true' ? 'false' : 'true';
}
