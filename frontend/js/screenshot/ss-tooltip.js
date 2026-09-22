//! 0.23.19：截图 overlay 自定义 tooltip（替换原生 title 提示）。
//!
//! 动机：原生 title 提示的落点由浏览器/OS 计算，在本窗口（跨整个虚拟桌面
//! + 工具栏 transform: scale 的混合 DPI 环境）里会漂到别的显示器——多屏下
//! 「工具类的提示文本定位到另外的屏幕」即由此而来，且原生提示无法钳制。
//! WebView2 按**单一窗口 DPI** 换算提示偏移，混合 DPI 布局下换算必然错位。
//!
//! 方案：单一共享 tooltip 元素挂在 body（不受工具栏 transform 影响），
//! 按锚点元素矩形定位（下方居中，空间不足上翻），钳制到锚点所在显示器，
//! 并按锚点所在屏做 UI scale 视觉补偿（spec §5.6）。
//! 交互节奏遵循悬浮层留消失延迟铁则（spec §5.3）：悬停 350ms 显示，
//! 离开留 150ms 缓冲，缓冲期内回到锚点即取消关闭。显隐带 0.12s 淡入淡出。
//!
//! 原生抑制（0.23.19 R3 重做）：初始化时**全量**把 [title] 迁移为 [data-tip]，
//! 并用 MutationObserver 常驻监听后续动态设置的 title——原生 tooltip 从结构上
//! 不可能出现，彻底消除"显示时才迁移"与原生弹出的竞态（此前动态设置 title
//! 的按钮会抢先弹原生提示）。

import {applyFloatingUiScaleAt, findDisplayCssAt} from './ss-display.js';

const SHOW_DELAY_MS = 350;
const HIDE_BUFFER_MS = 150;
const GAP = 6;

let tooltipEl = null;
let showTimer = 0;
let hideTimer = 0;
/** 当前显示中的锚点元素（null = 未显示） */
let currentAnchor = null;
/** 待显示计时中的锚点元素（null = 无待显示）。指针在同一锚点的子元素间
 *  移动会反复触发 pointerover，不跟踪它的话 350ms 计时会被不停重置。 */
let pendingAnchor = null;

function anchorOf(node) {
    // 同时匹配 [title] 与 [data-tip]——后者是迁移后的存放处；前者兜底
    // observer 尚未处理的瞬时窗口（同一 tick 内动态设置的 title）。
    return node && node.closest ? node.closest('[title], [data-tip]') : null;
}

/** 把单个元素的 title 属性迁移为 data-tip（抑制原生提示的唯一真源动作）。 */
function migrateTitle(el) {
    const title = el.getAttribute('title');
    if (!title) return;
    el.dataset.tip = title;
    el.removeAttribute('title');
}

/** 迁移 root 及其子树里的全部 [title]。 */
function migrateTitlesIn(root) {
    if (!root || root.nodeType !== 1) return;
    if (root.hasAttribute && root.hasAttribute('title')) migrateTitle(root);
    if (root.querySelectorAll) root.querySelectorAll('[title]').forEach(migrateTitle);
}

export function hideSsTooltip() {
    // 0.23.19 R4：必须同时清掉挂起的 hideTimer——从锚点 A 移到相邻锚点 B 时，
    // pointerout(A) 的 scheduleHide 已埋下 150ms 延迟隐藏，pointerover(B) 走到这里
    // 立即隐藏并给 B 重新调度显示；若旧 hideTimer 存活，会在 B 的 350ms 显示
    // 计时途中触发，把 showTimer 一并清掉——表现为"相邻按钮间移动气泡不再出现"。
    clearTimeout(showTimer);
    clearTimeout(hideTimer);
    pendingAnchor = null;
    if (tooltipEl) tooltipEl.classList.remove('is-visible');
    currentAnchor = null;
}

function scheduleHide() {
    clearTimeout(showTimer);
    pendingAnchor = null;
    clearTimeout(hideTimer);
    hideTimer = setTimeout(hideSsTooltip, HIDE_BUFFER_MS);
}

function show(anchor) {
    if (!tooltipEl || !anchor.isConnected) return;
    // 兜底：observer 之前的瞬时 title（正常路径已在迁移时转为 data-tip）
    migrateTitle(anchor);
    const tip = anchor.dataset.tip;
    if (!tip) return;
    currentAnchor = anchor;
    tooltipEl.textContent = tip;
    tooltipEl.classList.add('is-visible');

    const rect = anchor.getBoundingClientRect();
    const cx = rect.left + rect.width / 2;
    const cy = rect.top + rect.height / 2;
    // 按锚点所在屏做视觉补偿（tooltip 在 body 下，不在工具栏 transform 内）
    const uiScale = applyFloatingUiScaleAt(tooltipEl, cx, cy);
    const tw = tooltipEl.offsetWidth * uiScale;
    const th = tooltipEl.offsetHeight * uiScale;
    const mon = findDisplayCssAt(cx, cy);

    let left = cx - tw / 2;
    let top = rect.bottom + GAP;
    // 下方放不下（贴屏底）→ 上翻
    if (top + th > mon.y + mon.h - 4) top = rect.top - th - GAP;
    // 钳制到锚点所在显示器
    left = Math.max(mon.x + 4, Math.min(left, mon.x + mon.w - tw - 4));
    top = Math.max(mon.y + 4, Math.min(top, mon.y + mon.h - th - 4));
    tooltipEl.style.left = left + 'px';
    tooltipEl.style.top = top + 'px';
}

/** 初始化自定义 tooltip（幂等，由 index.js 装配时调用一次）。 */
export function initSsTooltip() {
    if (tooltipEl) return;
    tooltipEl = document.createElement('div');
    tooltipEl.id = 'ss-tooltip';
    tooltipEl.className = 'ss-tooltip';
    tooltipEl.setAttribute('aria-hidden', 'true');
    document.body.appendChild(tooltipEl);

    // 原生抑制第一道：存量 [title] 全量迁移（此后页面上不再有 title 属性）
    migrateTitlesIn(document.body);

    // 原生抑制第二道：动态设置 title（配色 swatch 更新文案、重建的控件等）
    // 一出现就迁移。removeAttribute 触发的回环记录里 title 已为空，天然终止。
    const titleObserver = new MutationObserver((records) => {
        for (const r of records) {
            if (r.type === 'attributes') {
                migrateTitle(r.target);
            } else {
                r.addedNodes.forEach(migrateTitlesIn);
            }
        }
    });
    titleObserver.observe(document.documentElement, {
        childList: true,
        subtree: true,
        attributes: true,
        attributeFilter: ['title'],
    });

    document.addEventListener('pointerover', (e) => {
        const anchor = anchorOf(e.target);
        // 缓冲期内回到当前锚点：取消关闭。仅在气泡确实显示中才走这条捷径——
        // 隐藏态（如待显示计时被取消后残留的 currentAnchor）须重新走显示调度。
        if (anchor === currentAnchor && tooltipEl.classList.contains('is-visible')) {
            clearTimeout(hideTimer);
            return;
        }
        if (anchor && anchor.contains(currentAnchor)) return; // 父子锚点间移动
        // 待显示计时中回到同一锚点（指针在其子元素间移动）：保持计时，不重置
        if (anchor && anchor === pendingAnchor) return;
        hideSsTooltip();
        if (!anchor) return;
        clearTimeout(showTimer);
        pendingAnchor = anchor;
        showTimer = setTimeout(() => {
            pendingAnchor = null;
            show(anchor);
        }, SHOW_DELAY_MS);
    });

    document.addEventListener('pointerout', (e) => {
        const next = e.relatedTarget;
        if (currentAnchor) {
            if (next && (anchorOf(next) === currentAnchor || currentAnchor.contains(next))) return;
            scheduleHide();
            return;
        }
        // 无当前锚点但有待显示计时——指针已离开锚点，必须取消，
        // 否则 350ms 后会在旧位置弹出"幽灵气泡"（指针早已不在那里）。
        // 移向另一个锚点时不取消：它的 pointerover 会重新调度。
        if (!next || !anchorOf(next)) {
            clearTimeout(showTimer);
            pendingAnchor = null;
        }
    });

    // 任何按下/滚轮/ESC 都立即收起——提示不应遮挡交互反馈
    document.addEventListener('pointerdown', hideSsTooltip, true);
    document.addEventListener('wheel', hideSsTooltip, {passive: true, capture: true});
    document.addEventListener('keydown', (e) => {
        if (e.key === 'Escape') hideSsTooltip();
    }, true);
}
