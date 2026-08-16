//! canvas-backed 编辑器来源显示器居中 + sizeHint 生命周期测试。
//!
//! 覆盖：
//! 1. computeCanvasEditorInitialPosition 居中到来源显示器
//! 2. computeCanvasEditorInitialPosition 副屏负 origin
//! 3. computeCanvasEditorInitialPosition 图片超出显示器
//! 4. ImageEditorSession.canvasBacked 标志（drawFinalSelection 据此跳过 sizeHint）
//! 5. ScrollCaptureSession.scrollSourceMonitor 字段存在性

import assert from 'node:assert/strict';
import {computeCanvasEditorInitialPosition} from './ss-utils.js';
import {IMAGE_SOURCE, ImageEditorSession} from './image-editor-session.js';
import {ScrollCaptureSession} from './scroll/session.js';

// ── 1. 居中到来源显示器（单屏） ──────────────────────────────────────

{
    const pos = computeCanvasEditorInitialPosition(400, 300, {x: 0, y: 0, w: 1920, h: 1080});
    assert.equal(pos.x, 760, '单屏居中 X = (1920-400)/2 = 760');
    assert.equal(pos.y, 390, '单屏居中 Y = (1080-300)/2 = 390');
    console.log('✓ computeCanvasEditorInitialPosition: 单屏居中');
}

// ── 2. 副屏负 origin 居中 ──────────────────────────────────────────────

// 副屏在主屏左侧 origin=(-1920, 0)
{
    const pos = computeCanvasEditorInitialPosition(400, 300, {x: -1920, y: 0, w: 1920, h: 1080});
    assert.equal(pos.x, -1920 + 760, '副屏左侧居中 X = -1920 + 760 = -1160');
    assert.equal(pos.y, 390, '副屏左侧居中 Y = 390');
    console.log('✓ computeCanvasEditorInitialPosition: 副屏左侧负 origin 居中');
}

// 副屏在主屏上方 origin=(0, -1080)
{
    const pos = computeCanvasEditorInitialPosition(400, 300, {x: 0, y: -1080, w: 1920, h: 1080});
    assert.equal(pos.x, 760, '副屏上方居中 X = 760');
    assert.equal(pos.y, -1080 + 390, '副屏上方居中 Y = -1080 + 390 = -690');
    console.log('✓ computeCanvasEditorInitialPosition: 副屏上方负 origin 居中');
}

// ── 3. 图片超出来源显示器 ──────────────────────────────────────────────

// 长图高 1500 > 屏高 1080
{
    const pos = computeCanvasEditorInitialPosition(400, 1500, {x: 0, y: 0, w: 1920, h: 1080});
    assert.equal(pos.x, 760, '宽度小于屏：X 仍居中');
    assert.equal(pos.y, 12, '高度大于屏：Y = 12（顶部保留 12px）');
    console.log('✓ computeCanvasEditorInitialPosition: 图片超出显示器');
}

// 长图宽 3000 > 屏宽 1920, 高 1500 > 屏高 1080
{
    const pos = computeCanvasEditorInitialPosition(3000, 1500, {x: 0, y: 0, w: 1920, h: 1080});
    assert.equal(pos.x, 12, '宽度大于屏：X = 12');
    assert.equal(pos.y, 12, '高度大于屏：Y = 12');
    console.log('✓ computeCanvasEditorInitialPosition: 宽高都超出显示器');
}

// 长图超出副屏（负 origin）
{
    const pos = computeCanvasEditorInitialPosition(400, 1500, {x: -1920, y: 0, w: 1920, h: 1080});
    assert.equal(pos.x, -1920 + 760, '宽度小于副屏：X 居中到副屏');
    assert.equal(pos.y, 12, '高度大于副屏：Y = 12');
    console.log('✓ computeCanvasEditorInitialPosition: 图片超出副屏（负 origin）');
}

// ── 4. ImageEditorSession.canvasBacked（drawFinalSelection 据此跳过 sizeHint） ──

{
    const session = new ImageEditorSession();
    assert.equal(session.canvasBacked, false, '初始状态：非 canvas-backed');

    // 模拟 beginCanvasSource
    const mockCanvas = {width: 800, height: 600};
    session.beginCanvasSource(IMAGE_SOURCE.LONG_SCREENSHOT, mockCanvas);
    assert.equal(session.canvasBacked, true, '长截图来源：canvas-backed = true');
    assert.equal(session.source, IMAGE_SOURCE.LONG_SCREENSHOT, '来源 = LONG_SCREENSHOT');

    session.reset();
    assert.equal(session.canvasBacked, false, 'reset 后：canvas-backed = false');

    session.beginCanvasSource(IMAGE_SOURCE.CLIPBOARD, mockCanvas);
    assert.equal(session.canvasBacked, true, '剪贴板来源：canvas-backed = true');
    console.log('✓ ImageEditorSession.canvasBacked: LONG_SCREENSHOT 和 CLIPBOARD 都为 true');
}

// ── 5. ScrollCaptureSession.scrollSourceMonitor 字段 ────────────────────

{
    const session = new ScrollCaptureSession();
    assert.equal(session.scrollSourceMonitor, null, '初始状态：scrollSourceMonitor = null');

    // 模拟设置
    session.scrollSourceMonitor = {x: -1920, y: 0, w: 1920, h: 1080};
    assert.deepEqual(session.scrollSourceMonitor, {x: -1920, y: 0, w: 1920, h: 1080}, '可设置来源显示器');

    // reset 清除
    session.reset();
    assert.equal(session.scrollSourceMonitor, null, 'reset 后：scrollSourceMonitor = null');
    console.log('✓ ScrollCaptureSession.scrollSourceMonitor: 字段存在且可被 reset 清除');
}

// ── 6. 长图高度介于当前屏高度和虚拟桌面高度之间 ──────────────────────

// 当前屏高 1080, 虚拟桌面高 2160（双屏上下）
// 长图高 1500：居中到当前屏，不从顶部开始
{
    const pos = computeCanvasEditorInitialPosition(400, 1500, {x: 0, y: 0, w: 1920, h: 1080});
    // 1500 > 1080 → Y = 12（超出屏幕，从顶部开始）
    // 但如果错误使用虚拟桌面 2160：1500 < 2160 → Y = (2160-1500)/2 = 330
    assert.equal(pos.y, 12, '长图介于屏高和虚拟桌面高之间：基于当前屏而非虚拟桌面');
    assert.notEqual(pos.y, 330, '不使用虚拟桌面高度居中');
    console.log('✓ computeCanvasEditorInitialPosition: 长图高度介于屏高和虚拟桌面高之间');
}

console.log('\n✅ 所有 canvas editor centering / sizeHint lifecycle 测试通过！');
