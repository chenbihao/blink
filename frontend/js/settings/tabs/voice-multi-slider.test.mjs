// 0.23.17 多手柄时序条纯逻辑测试：顶住约束 / 步长吸附 / 键盘步进 / 选区带
import assert from "node:assert/strict";

import {assignLabelRows, createTimespanModel, draftTargetBand} from "./voice-multi-slider.js";

// ── 顶住约束：切分窗口（soft < hard <= cap；hard 与 cap 可相等）──
{
    const model = createTimespanModel({
        max: 30,
        handles: [
            {key: "soft", min: 3, max: 30, step: 1, order: 1},
            {key: "hard", min: 5, max: 30, step: 1, order: 2, gapPrev: 1},
            {key: "cap", min: 5, max: 30, step: 1, order: 3, gapPrev: 0},
        ],
    });
    model.setValues({soft: 8, hard: 12, cap: 12});
    assert.deepEqual(model.getValues(), {soft: 8, hard: 12, cap: 12});

    // soft 拖过 hard-1 → 顶住在 hard-1（不推动 hard）
    assert.equal(model.setHandle("soft", 20), 11);
    assert.deepEqual(model.getValues(), {soft: 11, hard: 12, cap: 12});

    // hard 拖到 soft 之下 → 顶住在 soft+1
    assert.equal(model.setHandle("hard", 5), 12);
    // cap 下压 → 顶住在 hard（允许相等：默认 12/12 是不动点）
    assert.equal(model.setHandle("cap", 9), 12);
    // hard 上拖 → 顶住在 cap（可相等）
    assert.equal(model.setHandle("hard", 30), 12);
    // cap 单独下压到 hard 之下被顶回 hard
    const second = createTimespanModel({
        max: 30,
        handles: [
            {key: "soft", min: 3, max: 30, step: 1, order: 1},
            {key: "hard", min: 5, max: 30, step: 1, order: 2, gapPrev: 1},
            {key: "cap", min: 5, max: 30, step: 1, order: 3, gapPrev: 0},
        ],
    });
    second.setValues({soft: 8, hard: 12, cap: 12});
    assert.equal(second.setHandle("cap", 9), 12, "cap 不得低于 hard（可相等）");

    // 步长吸附：非网格值收敛
    const stepped = createTimespanModel({
        max: 2000,
        handles: [{key: "a", min: 100, max: 1000, step: 50, order: 1}],
    });
    stepped.setValues({a: 333});
    assert.equal(stepped.getValues().a, 350, "非网格值应吸附到 step 网格");
    assert.equal(stepped.setHandle("a", 9999), 1000, "越界钳到 max");
    assert.equal(stepped.setHandle("a", -5), 100, "越界钳到 min");
}

// ── 键盘步进 ──
{
    const model = createTimespanModel({
        max: 2000,
        handles: [{key: "a", min: 100, max: 1000, step: 50, order: 1}],
    });
    model.setValues({a: 300});
    assert.equal(model.nudge("a", 50), 350);
    assert.equal(model.nudge("a", -50), 300);
    assert.equal(model.nudge("a", -100000), 100, "向下步进不得越 min");
}

// ── order=0：无顶住关系（freeze 与 refresh/window 独立）──
{
    const model = createTimespanModel({
        max: 4000,
        handles: [
            {key: "refresh", min: 500, max: 1000, step: 50, order: 1},
            {key: "freeze", min: 0, max: 3000, step: 100, order: 0},
            {key: "window", min: 2000, max: 4000, step: 100, order: 2, gapPrev: 100},
        ],
    });
    model.setValues({refresh: 700, freeze: 3000, window: 3000});
    // freeze 可自由越过 refresh / window（无约束）
    assert.equal(model.setHandle("freeze", 0), 0);
    // refresh 自身边界 [500,1000] 先于邻居约束生效（window ≥ 2000 恒不约束）
    assert.equal(model.setHandle("refresh", 3000), 1000);
}

// ── 轴换算 / 最近手柄 ──
{
    const model = createTimespanModel({
        max: 2000,
        handles: [
            {key: "a", min: 100, max: 1000, step: 50, order: 1},
            {key: "b", min: 800, max: 2000, step: 50, order: 2},
        ],
    });
    model.setValues({a: 300, b: 1100});
    assert.equal(model.valueAtPosition(0), 0);
    assert.equal(model.valueAtPosition(1), 2000);
    assert.equal(model.valueAtPosition(1.5), 2000, "越界比例钳到 1");
    assert.equal(model.percentOf("a"), 15);
    assert.equal(model.nearestHandle(0.16), "a");
    assert.equal(model.nearestHandle(0.5), "b");
}

// ── 选区带（draft 目标窗口）──
{
    assert.deepEqual(draftTargetBand.range(24, 6), {start: 18, end: 30});
    assert.equal(draftTargetBand.range(0, 6), null, "target=0 关闭无选区");
    assert.equal(draftTargetBand.range(24, -1), null, "负宽容非法");
    assert.equal(draftTargetBand.toleranceFromEdge(30, 24), 6);
    assert.equal(draftTargetBand.toleranceFromEdge(18, 24), 6);
    assert.equal(draftTargetBand.toleranceFromEdge(NaN, 24), 0);
    assert.equal(draftTargetBand.toleranceFromEdge(30, 0), 0);
}

// ── 0.23.17 目标中心可达上界（与后端 sanitize 同一约束）──
{
    // 默认组合：cap 12 − 宽容 2 = 10（stock 下优选窗 [8, 12] 恰好可达）
    assert.equal(draftTargetBand.maxAchievableTarget(12, 2), 10);
    assert.equal(draftTargetBand.maxAchievableTarget(30, 6), 24);
    // 空间不足（cap − tolerance < 下限 4s）→ 不可达
    assert.equal(draftTargetBand.maxAchievableTarget(5, 10), 0);
    assert.equal(draftTargetBand.maxAchievableTarget(5, 2), 0, "3 < 下限 4 → 不可达");
    assert.equal(draftTargetBand.maxAchievableTarget(5, 1), 4, "恰好放下下限");
    // 无上限（未拿到 VAD 上限时）：只受自身边界约束
    assert.equal(draftTargetBand.maxAchievableTarget(Infinity, 2), 30);
    assert.equal(draftTargetBand.maxAchievableTarget(Infinity, 2, {minS: 4, maxS: 30}), 30);
    assert.equal(draftTargetBand.maxAchievableTarget(99, 1), 30, "不得越过中心安全上界");
    // 非法宽容按 0 处理（只在拖动中途出现）
    assert.equal(draftTargetBand.maxAchievableTarget(12, NaN), 12);
}

// ── 0.23.17 手柄标签分行（手柄靠近时标签不得叠字）──
{
    // 标签都宽 100px、轴 400px：25% 与 30% 相距 20px → 必须分行
    const tight = assignLabelRows([
        {key: "a", percent: 25, width: 100},
        {key: "b", percent: 30, width: 100},
    ], {trackWidth: 400, gapPx: 10});
    assert.deepEqual(tight, {rows: {a: 0, b: 1}, rowCount: 2});

    // 相距足够远 → 共处同一行（不浪费纵向空间）
    const spaced = assignLabelRows([
        {key: "a", percent: 10, width: 80},
        {key: "b", percent: 80, width: 80},
    ], {trackWidth: 400, gapPx: 10});
    assert.deepEqual(spaced, {rows: {a: 0, b: 0}, rowCount: 1});

    const clear = assignLabelRows([
        {key: "a", percent: 5, width: 40},
        {key: "b", percent: 50, width: 40},
        {key: "c", percent: 95, width: 40},
    ], {trackWidth: 400, gapPx: 10});
    assert.deepEqual(clear.rows, {a: 0, b: 0, c: 0}, "相距够远的标签共用一行");
    assert.equal(clear.rowCount, 1);

    // 三个手柄挤在左端：最多两行也放不下时继续增行（不叠字优先）
    const crowded = assignLabelRows([
        {key: "a", percent: 2, width: 90},
        {key: "b", percent: 3, width: 90},
        {key: "c", percent: 4, width: 90},
    ], {trackWidth: 400, gapPx: 10});
    assert.equal(new Set(Object.values(crowded.rows)).size, 3, "放不下就继续增行");

    // 轴宽未知（首帧布局前）：全部落在第一行，不得崩
    assert.deepEqual(assignLabelRows([{key: "a", percent: 50, width: 0}], {trackWidth: 0}),
        {rows: {a: 0}, rowCount: 1});
}

// ── 非法构造拒绝 ──
{
    assert.throws(() => createTimespanModel({max: 0, handles: [{key: "a", min: 1, max: 2}]}));
    assert.throws(() => createTimespanModel({max: 10, handles: []}));
    assert.throws(() => createTimespanModel({max: 10, handles: [{key: "a"}]}));
}

console.log("voice-multi-slider.test.mjs: all assertions passed");
