//! 实线选区实时层测试（0.23.15）。
//!
//! 覆盖交付要求的可断言条件：
//! 1. 同一帧收到多个坐标时，只渲染最后一个矩形
//! 2. 同时最多一个实时预览 rAF
//! 3. reset / 隐藏后旧 rAF 不得重新显示实时层（含"取消失效"的兜底路径）
//! 4. 实时层始终只有一个 border 元素，不通过 append 绘制（HTML/JS 双侧面契约）
//! 5. 拖动期间 interaction-canvas 不发生逐帧 clear/fill/stroke（只在激活时清空一次）
//! 6. 实时预览成本不随截图总像素面积线性增长
//! 7. CSS 像素几何契约；边框线宽按 monitorDpr/renderScale 补偿，
//!    且 devicePixelRatio 不作为坐标换算真源
//! 8. 尺寸提示与实时几何共用同一个 rAF，每帧最多更新一次
//! 9. 实时层的几何属性没有 transition / animation；预选虚线框动画未被改动

import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';

// ── 测试替身：最小元素 / 手动驱动的 rAF ───────────────────────────────────

/** 最小元素替身：记录 class、style 写入与 textContent 写入次数 */
function fakeElement(id) {
    const classes = new Set();
    const el = {
        id,
        classList: {
            add: (c) => classes.add(c),
            remove: (c) => classes.delete(c),
            contains: (c) => classes.has(c),
        },
        _classes: classes,
    };
    let text = '';
    let textWrites = 0;
    Object.defineProperty(el, 'textContent', {
        enumerable: true,
        get: () => text,
        set: (v) => {
            text = v;
            textWrites++;
        },
    });
    Object.defineProperty(el, 'textWrites', {get: () => textWrites});

    const styleRaw = {};
    let styleWrites = 0;
    el.style = new Proxy(styleRaw, {
        set(target, key, value) {
            target[key] = value;
            styleWrites++;
            return true;
        },
        get(target, key) {
            return typeof key === 'string' && key in target ? target[key] : '';
        },
    });
    Object.defineProperty(el, 'styleWrites', {get: () => styleWrites});
    return el;
}

let rafQueue = [];
let rafSeq = 0;
globalThis.requestAnimationFrame = (cb) => {
    const id = ++rafSeq;
    rafQueue.push({id, cb});
    return id;
};
globalThis.cancelAnimationFrame = (id) => {
    rafQueue = rafQueue.filter((f) => f.id !== id);
};

/** 手动跑一帧：只有真正排队过的回调会被执行 */
function runFrame() {
    const queue = rafQueue;
    rafQueue = [];
    for (const f of queue) f.cb();
}

globalThis.window = {
    // 故意设成与 renderScale / monitorDpr 都不一致：验证它不被当作坐标真源
    devicePixelRatio: 3,
    __blinkScreenMeta: {
        vx: 0, vy: 0,
        renderScaleX: 1, renderScaleY: 1,
        physicalDisplays: [{x: 0, y: 0, w: 3840, h: 2160, dpi: 96}],
    },
};

const {ss} = await import('./ss-state.js');
const live = await import('./ss-live-selection.js');

/** 装好整套替身；返回 interaction-canvas 的操作计数 */
function installDoubles() {
    // 每个场景从"静止态"开始：复位实时层（含在途 rAF 与线宽缓存）后换新替身
    live.resetLiveSelection();
    rafQueue = [];
    const ops = {clearRect: 0, fillRect: 0, strokeRect: 0};
    ss.canvas = {width: 3840, height: 2160};
    ss.interactionCanvas = {width: 3840, height: 2160};
    ss.interactionCtx = {
        clearRect: () => ops.clearRect++,
        fillRect: () => ops.fillRect++,
        strokeRect: () => ops.strokeRect++,
    };
    ss.liveSelectionEl = fakeElement('live-selection');
    ss.liveMaskTop = fakeElement('live-mask-top');
    ss.liveMaskBottom = fakeElement('live-mask-bottom');
    ss.liveMaskLeft = fakeElement('live-mask-left');
    ss.liveMaskRight = fakeElement('live-mask-right');
    ss.liveBorderEl = fakeElement('live-border');
    ss.sizeHint = fakeElement('size-hint');
    ss.liveSelectionEl._classes.add('hidden');
    return ops;
}

/** 每帧 DOM 几何写入总数（遮罩 + 边框） */
function frameStyleWrites() {
    return ss.liveMaskTop.styleWrites + ss.liveMaskBottom.styleWrites
        + ss.liveMaskLeft.styleWrites + ss.liveMaskRight.styleWrites
        + ss.liveBorderEl.styleWrites;
}

// ── 1/5：同一帧多个坐标只渲染最后一个；interaction-canvas 全程零绘制 ──────

{
    const ops = installDoubles();
    live.updateLiveSelection({x: 1, y: 2, w: 3, h: 4});
    assert.equal(live.isLiveSelectionActive(), true, '首次更新即激活实时层');
    assert.equal(ops.clearRect, 1, '激活时清空 interaction-canvas 恰好一次');
    assert.equal(live.getPendingLiveRect(), null, '首帧同步落地，不留 pending');
    assert.equal(!ss.liveSelectionEl._classes.has('hidden'), true, '激活后实时层可见');

    live.updateLiveSelection({x: 10, y: 10, w: 20, h: 20});
    live.updateLiveSelection({x: 30, y: 40, w: 50, h: 60});
    live.updateLiveSelection({x: 7, y: 7, w: 7, h: 7});
    assert.equal(rafQueue.length, 1, '同一帧最多一个实时预览 rAF');
    assert.deepEqual(live.getPendingLiveRect(), {x: 7, y: 7, w: 7, h: 7}, 'pending 只保留最新矩形');

    runFrame();
    assert.equal(ss.liveBorderEl.style.left, '7px', '边框 left = 最新矩形 x');
    assert.equal(ss.liveBorderEl.style.top, '7px', '边框 top = 最新矩形 y');
    assert.equal(ss.liveBorderEl.style.width, '7px', '边框 width = 最新矩形 w');
    assert.equal(ss.liveBorderEl.style.height, '7px', '边框 height = 最新矩形 h');
    assert.equal(live.getPendingLiveRect(), null, '渲染后清空 pending');

    // 连续 120 帧高频拖动：interaction-canvas 仍然一动不动
    for (let i = 0; i < 120; i++) {
        live.updateLiveSelection({x: i, y: i, w: 100, h: 100});
        runFrame();
    }
    assert.equal(ops.clearRect, 1, '拖动期间 interaction-canvas clearRect 不再发生');
    assert.equal(ops.fillRect, 0, '拖动期间 interaction-canvas fillRect 为 0');
    assert.equal(ops.strokeRect, 0, '拖动期间 interaction-canvas strokeRect 为 0');
    assert.equal(ss.liveBorderEl.style.left, '119px', '最后一帧几何已落地');

    live.hideLiveSelection();
    assert.equal(live.isLiveSelectionActive(), false, 'hide 后回到非激活态');
    assert.ok(ss.liveSelectionEl._classes.has('hidden'), 'hide 后实时层隐藏');
    // 幂等：重复 hide 不炸、不改变状态
    live.hideLiveSelection();
    assert.equal(live.isLiveSelectionActive(), false, 'hide 幂等');
    console.log('✓ 单飞调度 / 零 canvas 绘制');
}

// ── 3：reset 后旧 rAF 不得重新显示实时层（含取消失效的兜底路径） ──────────

{
    installDoubles();
    live.updateLiveSelection({x: 5, y: 5, w: 5, h: 5});
    live.updateLiveSelection({x: 9, y: 9, w: 9, h: 9});
    assert.equal(rafQueue.length, 1, '已排一帧');
    const staleCallback = rafQueue[rafQueue.length - 1].cb;

    live.resetLiveSelection();
    assert.equal(live.isLiveSelectionActive(), false, 'reset 后非激活');
    assert.ok(ss.liveSelectionEl._classes.has('hidden'), 'reset 后实时层隐藏');
    assert.equal(ss.liveBorderEl.style.left, '0px', 'reset 后几何清零');
    assert.equal(ss.liveMaskTop.style.height, '0px', 'reset 后遮罩几何清零');
    assert.equal(rafQueue.length, 0, 'reset 取消了待执行 rAF');

    // 兜底：即使调度器漏取消（浏览器回收顺序异常），旧回调落地也必须被代际守卫拦下
    staleCallback();
    assert.equal(ss.liveBorderEl.style.left, '0px', '旧 rAF 不得重新写几何');
    assert.ok(ss.liveSelectionEl._classes.has('hidden'), '旧 rAF 不得重新显示实时层');

    // 旧回调不得破坏新会话的调度状态
    live.updateLiveSelection({x: 11, y: 11, w: 11, h: 11});
    assert.equal(live.isLiveSelectionActive(), true, '可重新激活');
    assert.equal(rafQueue.length, 0, '重新激活走同步首帧');
    live.updateLiveSelection({x: 12, y: 12, w: 12, h: 12});
    assert.equal(rafQueue.length, 1, '新交互仍能正常排帧');
    runFrame();
    assert.equal(ss.liveBorderEl.style.left, '12px', '新交互几何正常落地');
    live.resetLiveSelection();
    console.log('✓ reset / 旧 rAF 代际守卫');
}

// ── 6：成本不随截图总像素面积线性增长 ────────────────────────────────────

{
    // 单屏 1080p 与双 4K 虚拟桌面：每帧 DOM 写入次数必须完全一致，
    // 且 interaction-canvas 在这两种尺寸下的绘制次数都为 0
    const measure = (w, h) => {
        installDoubles();
        ss.canvas = {width: w, height: h};
        ss.interactionCanvas = {width: w, height: h};
        live.updateLiveSelection({x: 10, y: 10, w: 200, h: 150});
        const baseline = frameStyleWrites();
        let writes = 0;
        for (let i = 0; i < 10; i++) {
            live.updateLiveSelection({x: 10 + i, y: 10, w: 200, h: 150});
            const before = frameStyleWrites();
            runFrame();
            writes += frameStyleWrites() - before;
        }
        live.resetLiveSelection();
        return {baseline, writes};
    };
    const small = measure(1920, 1080);
    const huge = measure(7680, 4320);
    assert.equal(huge.writes, small.writes, '每帧 DOM 写入与截图总像素面积无关');
    assert.ok(small.writes > 0, '确实发生了几何写入');
    console.log('✓ 成本与截图面积解耦');
}

// ── 7：CSS 像素几何契约 + 跨屏线宽补偿 + devicePixelRatio 不参与换算 ──────

{
    installDoubles();
    live.updateLiveSelection({x: 100, y: 50, w: 200, h: 120});
    assert.equal(ss.liveMaskTop.style.height, '50px', '上遮罩 height = y');
    assert.equal(ss.liveMaskBottom.style.top, '170px', '下遮罩 top = y + h');
    assert.equal(ss.liveMaskLeft.style.top, '50px', '左遮罩 top = y');
    assert.equal(ss.liveMaskLeft.style.height, '120px', '左遮罩 height = h');
    assert.equal(ss.liveMaskLeft.style.width, '100px', '左遮罩 width = x');
    assert.equal(ss.liveMaskRight.style.top, '50px', '右遮罩 top = y');
    assert.equal(ss.liveMaskRight.style.height, '120px', '右遮罩 height = h');
    assert.equal(ss.liveMaskRight.style.left, '300px', '右遮罩 left = x + w');
    assert.equal(ss.liveBorderEl.style.width, '200px', '边框 width = w');
    assert.equal(ss.liveBorderEl.style.height, '120px', '边框 height = h');
    // devicePixelRatio=3 不得进入几何：mask 高度仍是 CSS 的 y
    assert.equal(ss.liveMaskTop.style.height, '50px', 'devicePixelRatio 不参与几何换算');
    // renderScale=1 + monitorDpr=1 → uiScale=1 → 线宽 2px
    assert.equal(ss.liveBorderEl.style.borderWidth, '2px', '1× 屏线宽 2px');

    // 跨屏补偿：renderScale=1.5 且目标屏 200%（dpi=192）→ uiScale=2/1.5
    window.__blinkScreenMeta = {
        vx: 0, vy: 0,
        renderScaleX: 1.5, renderScaleY: 1.5,
        physicalDisplays: [{x: 0, y: 0, w: 3840, h: 2160, dpi: 192}],
    };
    installDoubles();
    live.updateLiveSelection({x: 10, y: 10, w: 100, h: 100});
    const expected = 2 * (192 / 96) / 1.5;
    assert.ok(
        Math.abs(parseFloat(ss.liveBorderEl.style.borderWidth) - expected) < 1e-6,
        `200% 屏 + renderScale 1.5 线宽补偿为 ${expected}px，实际 ${ss.liveBorderEl.style.borderWidth}`,
    );
    assert.equal(ss.liveBorderEl.style.width, '100px', '线宽补偿不改几何');
    live.resetLiveSelection();

    // 线宽未变化时不再重复写（逐帧同屏拖动不产生多余样式写入）
    window.__blinkScreenMeta = {
        vx: 0, vy: 0,
        renderScaleX: 1, renderScaleY: 1,
        physicalDisplays: [{x: 0, y: 0, w: 3840, h: 2160, dpi: 96}],
    };
    installDoubles();
    live.updateLiveSelection({x: 0, y: 0, w: 10, h: 10});
    const afterFirst = ss.liveBorderEl.styleWrites;
    live.updateLiveSelection({x: 0, y: 0, w: 20, h: 20});
    runFrame();
    const perFrameAfterFirst = ss.liveBorderEl.styleWrites - afterFirst;
    assert.equal(perFrameAfterFirst, 4, '线宽未变时边框每帧只写 left/top/width/height');
    live.resetLiveSelection();

    // 负坐标：遮罩宽高被钳制、边框位置保留（与 canvas strokeRect 语义一致）
    installDoubles();
    live.updateLiveSelection({x: -30, y: -40, w: 100, h: 100});
    assert.equal(ss.liveMaskLeft.style.width, '0px', '遮罩宽高不得为负');
    assert.equal(ss.liveMaskTop.style.height, '0px', '遮罩高不得为负');
    assert.equal(ss.liveBorderEl.style.left, '-30px', '边框 left 允许为负');
    assert.equal(ss.liveBorderEl.style.top, '-40px', '边框 top 允许为负');
    assert.equal(ss.liveBorderEl.style.width, '100px', '边框宽仍为正');
    live.resetLiveSelection();
    console.log('✓ CSS 像素几何 + 跨屏线宽补偿');
}

// ── 8：尺寸提示与实时几何共用同一个 rAF，每帧最多更新一次 ─────────────────

{
    installDoubles();
    live.updateLiveSelection({x: 40, y: 60, w: 400, h: 300});
    assert.equal(ss.sizeHint.textWrites, 1, '激活首帧写一次尺寸提示');
    assert.match(ss.sizeHint.textContent, /\(40, 60\) 400 × 300 px/, '尺寸提示为物理像素尺寸 + 屏幕坐标');
    assert.ok(!ss.sizeHint._classes.has('hidden'), '尺寸提示显示');

    const writesAfterFirst = ss.sizeHint.textWrites;
    live.updateLiveSelection({x: 50, y: 60, w: 400, h: 300});
    live.updateLiveSelection({x: 90, y: 60, w: 400, h: 300});
    runFrame();
    assert.equal(ss.sizeHint.textWrites - writesAfterFirst, 1, '一帧内尺寸提示最多写一次');
    assert.match(ss.sizeHint.textContent, /\(90, 60\)/, '尺寸提示消费同一帧的最新矩形');
    assert.equal(ss.sizeHint.style.left, '94px', '尺寸提示 left = x + 4');
    live.resetLiveSelection();
    console.log('✓ 尺寸提示与实时几何同帧');
}

// ── 4/9：DOM、HTML、CSS 三方契约 ─────────────────────────────────────────

{
    const html = readFileSync(new URL('../../chord-screenshot.html', import.meta.url), 'utf8');
    const css = readFileSync(new URL('../../css/views/chord-screenshot.css', import.meta.url), 'utf8');
    const src = readFileSync(new URL('./ss-live-selection.js', import.meta.url), 'utf8');

    const countOf = (haystack, needle) => haystack.split(needle).length - 1;

    assert.equal(countOf(html, 'id="live-selection"'), 1, '实时层容器唯一');
    assert.equal(countOf(html, 'class="live-selection-mask"'), 4, '遮罩恰好四块');
    assert.equal(countOf(html, 'class="live-selection-border"'), 1, '实线边框恰好一个元素');
    assert.equal(countOf(html, 'id="live-border"'), 1, '边框 id 唯一');

    const idxInteraction = html.indexOf('id="interaction-canvas"');
    const idxLive = html.indexOf('id="live-selection"');
    const idxAnnot = html.indexOf('id="annot-canvas"');
    assert.ok(idxInteraction >= 0 && idxLive > idxInteraction, '实时层在 interaction-canvas 之后');
    assert.ok(idxAnnot > idxLive, '实时层在 annot-canvas 之前');

    // 实时层只写 style、不建元素：源文件里不得出现动态建 DOM / 追加子节点
    assert.equal(/createElement|appendChild|insertAdjacentHTML/.test(src), false,
        '实时层不得通过 append 新元素绘制');

    // 实时层 CSS：几何禁止过渡/动画；层级在 interaction-canvas 之上、annot-canvas 之下
    const liveCssStart = css.indexOf('0.23.15：实线选区实时层');
    const liveCssEnd = css.indexOf('/* 标注 canvas');
    assert.ok(liveCssStart > 0 && liveCssEnd > liveCssStart, '找到实时层 CSS 区块');
    const liveCss = css.slice(liveCssStart, liveCssEnd);
    assert.equal(/transition\s*:/.test(liveCss), false, '实时层几何不得有 transition');
    assert.equal(/animation\s*:/.test(liveCss), false, '实时层几何不得有 animation');
    assert.equal(/@keyframes/.test(liveCss), false, '实时层不得定义关键帧');
    assert.match(liveCss, /pointer-events:\s*none/, '实时层必须 pointer-events:none');
    assert.match(liveCss, /z-index:\s*1/, '实时层 z-index 与 interaction-canvas 同级并靠 DOM 顺序压住');

    // 颜色/线宽常量与 CSS 真源对拍：这是 LIVE_SELECTION_STYLE 存在的唯一理由——
    // 遮罩色与边框色写在 CSS 里、被 JS 常量镜像，任何一侧单独改动都会在这里失败。
    assert.ok(css.includes(`background: ${live.LIVE_SELECTION_STYLE.maskColor}`),
        `遮罩色 CSS 与 JS 常量一致（${live.LIVE_SELECTION_STYLE.maskColor}）`);
    assert.ok(css.includes(`solid ${live.LIVE_SELECTION_STYLE.borderColor}`),
        `边框色 CSS 与 JS 常量一致（${live.LIVE_SELECTION_STYLE.borderColor}）`);
    assert.ok(css.includes(`--live-border-width: ${live.LIVE_SELECTION_STYLE.borderCssWidth}px`),
        `实时层默认线宽 CSS 与 JS 常量一致（${live.LIVE_SELECTION_STYLE.borderCssWidth}px）`);

    // 禁止修改预选虚线框：130ms 形变动画必须原样保留
    const hintCssStart = css.indexOf('.preselection-hint {');
    assert.ok(hintCssStart > 0, '找到预选虚线框样式');
    const hintCss = css.slice(hintCssStart, css.indexOf('.preselection-hint--control'));
    assert.match(hintCss, /transition:\s*left 0\.13s/, '预选虚线框 130ms 形变动画未被改动');
    assert.match(hintCss, /border:\s*3px dashed/, '预选虚线框颜色/线宽未被改动');
    assert.match(hintCss, /opacity:\s*0/, '预选虚线框隐藏策略未被改动');

    console.log('✓ DOM / HTML / CSS 契约');
}

// ── 2/状态机收尾 ─────────────────────────────────────────────────────────

{
    installDoubles();
    assert.equal(live.isLiveSelectionActive(), false, '初始非激活');
    live.resetLiveSelection();
    assert.equal(live.isLiveSelectionActive(), false, '未激活时 reset 安全');
    assert.equal(live.getPendingLiveRect(), null, '未激活时无 pending');
    // 空矩形不激活（避免无效 DOM 操作）
    live.updateLiveSelection(null);
    assert.equal(live.isLiveSelectionActive(), false, 'null 矩形不入队');
    console.log('✓ 状态机边界');
}

// ── 10：DOM 层缺失时的降级——不激活、不刷屏 ───────────────────────────────

{
    installDoubles();
    // 破坏契约：只缺一个元素就足以判定 DOM 层不可用
    ss.liveSelectionEl = null;

    const warned = [];
    const originalWarn = console.warn;
    console.warn = (...args) => warned.push(args.join(' '));
    try {
        for (let i = 0; i < 5; i++) {
            live.updateLiveSelection({x: i, y: 0, w: 10, h: 10});
        }
    } finally {
        console.warn = originalWarn;
    }

    assert.equal(live.isLiveSelectionActive(), false, 'DOM 缺失时不激活（不写几何、不清画布）');
    assert.equal(live.getPendingLiveRect(), null, 'DOM 缺失时不排队 rAF');
    assert.equal(warned.length, 1, '连续 5 次尝试只告警一次，避免每帧刷控制台');
    assert.match(warned[0], /DOM 层缺失/, '告警内容指出契约破坏');
    installDoubles();
    console.log('✓ DOM 缺失降级（告警一次）');
}

console.log('\nss-live-selection tests all passed');
