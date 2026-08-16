//! input-state-core 纯逻辑测试（0.18.7 Phase D）。
//!
//! 运行：node --test frontend/js/main-window/input-state-core.test.mjs

import {describe, test} from "node:test";
import assert from "node:assert/strict";

import {createInputStateCore} from "./input-state-core.js";

describe("applyState — UI revision 去重", () => {
    test("首次接受任何 revision（含 0）", () => {
        const core = createInputStateCore();
        assert.ok(core.applyState({revision: 0, altDown: false, windowVisible: false, exclusiveChordActive: false}));
        assert.equal(core.state.revision, 0);
    });

    test("更大 revision 被接受", () => {
        const core = createInputStateCore();
        core.applyState({revision: 1, altDown: false, windowVisible: false, exclusiveChordActive: false});
        assert.ok(core.applyState({revision: 5, altDown: true, windowVisible: true, exclusiveChordActive: true}));
        assert.equal(core.state.revision, 5);
        assert.equal(core.state.altDown, true);
    });

    test("相同 revision 被拒绝", () => {
        const core = createInputStateCore();
        core.applyState({revision: 3, altDown: false, windowVisible: false, exclusiveChordActive: false});
        assert.ok(!core.applyState({revision: 3, altDown: true, windowVisible: true, exclusiveChordActive: true}));
        assert.equal(core.state.altDown, false); // 保持旧状态
    });

    test("更小 revision 被拒绝（乱序事件）", () => {
        const core = createInputStateCore();
        core.applyState({revision: 10, altDown: true, windowVisible: true, exclusiveChordActive: true});
        assert.ok(!core.applyState({revision: 5, altDown: false, windowVisible: false, exclusiveChordActive: false}));
        assert.equal(core.state.altDown, true); // 保持新状态
    });

    test("event/snapshot 乱序：先收到快照再收到旧事件不覆盖", () => {
        const core = createInputStateCore();
        // register 返回快照 revision=5
        core.applyState({revision: 5, altDown: true, windowVisible: true, exclusiveChordActive: false});
        // 后到达的旧事件 revision=3
        assert.ok(!core.applyState({revision: 3, altDown: false, windowVisible: false, exclusiveChordActive: false}));
        assert.equal(core.state.altDown, true);
    });

    test("null / 非法 state 被拒绝", () => {
        const core = createInputStateCore();
        assert.ok(!core.applyState(null));
        assert.ok(!core.applyState(undefined));
        assert.ok(!core.applyState({}));
    });
});

describe("updateContext — context change 去重", () => {
    test("首次更新返回 context", () => {
        const core = createInputStateCore();
        core.setViewEpoch(1);
        const ctx = core.updateContext(false, false, false);
        assert.ok(ctx);
        assert.equal(ctx.viewEpoch, 1);
        assert.equal(ctx.revision, 1);
        assert.equal(ctx.queryEmpty, false);
        assert.equal(ctx.aiMode, false);
        assert.equal(ctx.clipboardMode, false);
    });

    test("相同值不产生上报", () => {
        const core = createInputStateCore();
        core.setViewEpoch(1);
        core.updateContext(false, true, false);
        assert.equal(core.updateContext(false, true, false), null);
    });

    test("仅 queryEmpty 变化产生上报", () => {
        const core = createInputStateCore();
        core.setViewEpoch(1);
        core.updateContext(false, false, false);
        const ctx = core.updateContext(true, false, false);
        assert.ok(ctx);
        assert.equal(ctx.queryEmpty, true);
        assert.equal(ctx.aiMode, false);
        assert.equal(ctx.clipboardMode, false);
        assert.equal(ctx.revision, 2);
    });

    test("仅 aiMode 变化产生上报", () => {
        const core = createInputStateCore();
        core.setViewEpoch(1);
        core.updateContext(false, false, false); // queryEmpty 变化 → revision=1
        const ctx = core.updateContext(false, true, false); // 仅 aiMode 变化 → revision=2
        assert.ok(ctx);
        assert.equal(ctx.queryEmpty, false);
        assert.equal(ctx.aiMode, true);
        assert.equal(ctx.clipboardMode, false);
        assert.equal(ctx.revision, 2);
    });

    test("仅 clipboardMode 变化产生上报（0.20.8）", () => {
        const core = createInputStateCore();
        core.setViewEpoch(1);
        core.updateContext(false, false, false); // revision=1
        const ctx = core.updateContext(false, false, true); // 仅 clipboardMode 变化 → revision=2
        assert.ok(ctx);
        assert.equal(ctx.queryEmpty, false);
        assert.equal(ctx.aiMode, false);
        assert.equal(ctx.clipboardMode, true);
        assert.equal(ctx.revision, 2);
    });

    test("revision 递增", () => {
        const core = createInputStateCore();
        core.setViewEpoch(1);
        let ctx = core.updateContext(false, false, false);
        assert.equal(ctx.revision, 1);
        ctx = core.updateContext(true, false, false);
        assert.equal(ctx.revision, 2);
        ctx = core.updateContext(true, true, false);
        assert.equal(ctx.revision, 3);
        ctx = core.updateContext(true, true, true);
        assert.equal(ctx.revision, 4);
        // 无变化不递增
        assert.equal(core.updateContext(true, true, true), null);
        ctx = core.updateContext(false, false, false);
        assert.equal(ctx.revision, 5);
    });
});

describe("旧 view epoch 丢弃", () => {
    test("reset 后 viewEpoch 归 0", () => {
        const core = createInputStateCore();
        core.setViewEpoch(5);
        core.updateContext(false, false, false);
        core.reset();
        assert.equal(core.viewEpoch, 0);
        assert.equal(core.state, null);
    });

    test("reset 后重新 setViewEpoch 可正常工作", () => {
        const core = createInputStateCore();
        core.setViewEpoch(1);
        core.updateContext(false, false, false);
        core.reset();
        core.setViewEpoch(2);
        const ctx = core.updateContext(false, false, false);
        assert.ok(ctx);
        assert.equal(ctx.viewEpoch, 2);
        assert.equal(ctx.revision, 1);
    });
});

describe("setViewEpoch — 初始化", () => {
    test("设置 epoch 后首次 updateContext revision=1", () => {
        const core = createInputStateCore();
        core.setViewEpoch(42);
        assert.equal(core.viewEpoch, 42);
        const ctx = core.updateContext(false, false, false);
        assert.ok(ctx);
        assert.equal(ctx.revision, 1);
    });
});
