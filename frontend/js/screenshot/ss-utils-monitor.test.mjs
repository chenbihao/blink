//! 多显示器混合 DPI 下 pan bounds 与浮动定位测试。
//!
//! 覆盖：
//! 1. computePanAxisBounds 非零 origin（副屏左侧/上方）
//! 2. computePanAxisBounds 图片超出当前显示器（保留 48px 可抓回）
//! 3. computeFloatingPlacement 非零 monitor origin clamp

import assert from 'node:assert/strict';
import {computeFloatingPlacement, computePanAxisBounds} from './ss-utils.js';

// ── 1. computePanAxisBounds：非零 origin ──────────────────────────────

// 副屏在主屏左侧，origin.x = -1920
// 图片宽 800 < 屏宽 1920：完整位于屏内
{
    const r = computePanAxisBounds(800, 1920, -1920);
    assert.equal(r.min, -1920, '图片小于屏：min = origin');
    assert.equal(r.max, -1920 + 1920 - 800, '图片小于屏：max = origin + viewport - image');
    console.log('✓ computePanAxisBounds: 副屏左侧负 origin，图片小于屏');
}

// 副屏在主屏上方，origin.y = -1080
// 图片高 800 < 屏高 1080：完整位于屏内
{
    const r = computePanAxisBounds(800, 1080, -1080);
    assert.equal(r.min, -1080, '副屏上方负 origin：min = origin');
    assert.equal(r.max, -1080 + 1080 - 800, '副屏上方负 origin：max = origin + viewport - image');
    console.log('✓ computePanAxisBounds: 副屏上方负 origin，图片小于屏');
}

// ── 2. computePanAxisBounds：图片超出当前显示器 ─────────────────────────

// 长图高 3000 > 屏高 1080，origin.y = 0
// 保留 48px 可抓回
{
    const r = computePanAxisBounds(3000, 1080, 0);
    assert.equal(r.min, 48 - 3000, '图片大于屏：min = origin + minVisible - imageSize');
    assert.equal(r.max, 1080 - 48, '图片大于屏：max = origin + viewportSize - minVisible');
    console.log('✓ computePanAxisBounds: 图片超出当前显示器，保留 48px');
}

// 长图高 3000 > 屏高 1080，副屏 origin.y = -1080
{
    const r = computePanAxisBounds(3000, 1080, -1080);
    assert.equal(r.min, -1080 + 48 - 3000, '副屏负 origin + 图片超出');
    assert.equal(r.max, -1080 + 1080 - 48, '副屏负 origin + 图片超出');
    console.log('✓ computePanAxisBounds: 副屏负 origin + 图片超出当前显示器');
}

// ── 3. computeFloatingPlacement：非零 monitor origin clamp ──────────────

// 副屏在左侧 origin=(-1920, 0, 1920, 1080)
// 锚选区在副屏上，工具栏应 clamp 到副屏内
{
    const placement = computeFloatingPlacement({
        anchorRect: {x: -1500, y: 100, w: 400, h: 300},
        visualWidth: 200, visualHeight: 40,
        monitorRect: {x: -1920, y: 0, w: 1920, h: 1080},
        margin: 8,
        preferred: 'below-center',
    });
    // 工具栏中心 = anchorRect.x + anchorRect.w / 2 = -1300
    // 工具栏 left = -1300 - 100 = -1400
    // 应在副屏内 [-1912, -1920+1920-200-8 = -208]
    assert.ok(placement.left >= -1912, `left ${placement.left} >= -1912`);
    assert.ok(placement.left <= -208, `left ${placement.left} <= -208`);
    // below = 100 + 300 + 8 = 408, 在副屏内 [8, 1080-40-8=1032]
    assert.ok(placement.top >= 8, `top ${placement.top} >= 8`);
    assert.ok(placement.top <= 1032, `top ${placement.top} <= 1032`);
    console.log('✓ computeFloatingPlacement: 副屏负 origin clamp');
}

// 副屏在上方 origin=(0, -1080, 1920, 1080)
// 选区贴副屏底边，工具栏应在选区下方但 clamp 到副屏内
{
    const placement = computeFloatingPlacement({
        anchorRect: {x: 500, y: -300, w: 400, h: 300},
        visualWidth: 200, visualHeight: 40,
        monitorRect: {x: 0, y: -1080, w: 1920, h: 1080},
        margin: 8,
        preferred: 'below-center',
    });
    // below = -300 + 300 + 8 = 8, 在副屏内 [-1072, -1080+1080-40-8=-48]
    assert.ok(placement.top >= -1072, `top ${placement.top} >= -1072`);
    assert.ok(placement.top <= -48, `top ${placement.top} <= -48`);
    console.log('✓ computeFloatingPlacement: 副屏上方负 origin clamp');
}

// ── 4. 长图高度介于当前屏高度和虚拟桌面高度之间 ──────────────────────

// 当前屏高 1080，虚拟桌面高 2160（双屏上下排列）
// 长图高 1500 > 1080 但 < 2160
// pan bounds 应基于当前屏 1080，不是虚拟桌面 2160
{
    const r = computePanAxisBounds(1500, 1080, 0);
    // 1500 > 1080 → 允许拖出屏幕，保留 48px
    assert.equal(r.min, 48 - 1500, '长图介于屏高和虚拟桌面高之间：基于当前屏');
    assert.equal(r.max, 1080 - 48, '长图介于屏高和虚拟桌面高之间：基于当前屏');
    // 验证不是基于虚拟桌面
    assert.ok(r.max < 2160 - 48, 'max 不是基于虚拟桌面高度');
    console.log('✓ computePanAxisBounds: 长图高度介于屏高和虚拟桌面高之间');
}

console.log('\n✅ 所有 monitor pan bounds / placement 测试通过！');
