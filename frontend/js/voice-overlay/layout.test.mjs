/**
 * voice-overlay 窗口尺寸决策（0.23.7）测试。
 *
 * 覆盖：
 * - g2 模式沿用内容高度撑高 + 140/400 钳制（0.10.6 行为不回归）；
 * - editor 模式固定稳定尺寸，不依赖内容高度（首条识别文本到达前后一致）；
 * - editor mini 单独一档；
 * - 非法/缺失输入不产生 NaN。
 */

const {test} = await import("node:test");
const assert = (await import("node:assert/strict")).default;
const {
    EDITOR_MINI_SIZE,
    EDITOR_SIZE,
    G2_SIZE,
    resolveOverlaySize,
} = await import("./layout.js");

test("g2 模式：沿用内容高度撑高并按 140/400 钳制", () => {
    assert.deepEqual(resolveOverlaySize({mode: "g2", contentHeight: 120}), {
        width: G2_SIZE.width,
        height: G2_SIZE.minHeight,
    });
    assert.deepEqual(resolveOverlaySize({mode: "g2", contentHeight: 260}), {
        width: G2_SIZE.width,
        height: 260,
    });
    assert.deepEqual(resolveOverlaySize({mode: "g2", contentHeight: 900}), {
        width: G2_SIZE.width,
        height: G2_SIZE.maxHeight,
    });
});

test("editor 模式：固定稳定尺寸，不依赖内容高度（避免窗口持续跳动）", () => {
    for (const contentHeight of [0, 20, 150, 400, 900]) {
        assert.deepEqual(
            resolveOverlaySize({mode: "editor", contentHeight}),
            {...EDITOR_SIZE},
            `contentHeight=${contentHeight} 不应影响 editor 固定尺寸`,
        );
    }
});

test("editor mini：独立一档更小尺寸", () => {
    const mini = resolveOverlaySize({mode: "editor", mini: true, contentHeight: 400});
    assert.equal(mini.width, EDITOR_MINI_SIZE.width);
    assert.equal(mini.height, EDITOR_MINI_SIZE.height);
    assert.ok(mini.height < EDITOR_SIZE.height, "mini 高度应小于展开态");
});

test("editor 尺寸落在任务约定的舒适区间（300-320 宽 / 190-220 高）", () => {
    assert.ok(EDITOR_SIZE.width >= 300 && EDITOR_SIZE.width <= 320);
    assert.ok(EDITOR_SIZE.height >= 190 && EDITOR_SIZE.height <= 220);
});

test("非法/缺失输入回退到安全值，不产生 NaN", () => {
    assert.deepEqual(resolveOverlaySize({mode: "g2"}), {
        width: G2_SIZE.width,
        height: G2_SIZE.minHeight,
    });
    assert.deepEqual(resolveOverlaySize({mode: "g2", contentHeight: NaN}), {
        width: G2_SIZE.width,
        height: G2_SIZE.minHeight,
    });
    assert.deepEqual(resolveOverlaySize({mode: "editor"}), {...EDITOR_SIZE});
    // 未知模式按 g2 处理
    assert.deepEqual(resolveOverlaySize({mode: "unknown", contentHeight: 200}), {
        width: G2_SIZE.width,
        height: 200,
    });
});
