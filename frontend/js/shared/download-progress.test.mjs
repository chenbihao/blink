/**
 * 下载进度纯函数测试（0.22.14）。
 *
 * 覆盖：样本窗口（有界/字节回退重置）、ETA 估算（样本不足/总量未知/
 * 停滞/超时不可信）、ETA 分桶取词参数、字节格式化、百分比钳制。
 */

import assert from "node:assert/strict";
import {
    estimateEtaMs,
    etaTextKeyAndParams,
    formatBytes,
    progressPercent,
    pushProgressSample,
} from "./download-progress.js";

// ── pushProgressSample：追加 + 有界窗口 ──────────────────────────────────────

{
    let s = pushProgressSample([], 0, 100);
    s = pushProgressSample(s, 200, 200);
    s = pushProgressSample(s, 400, 300);
    assert.equal(s.length, 3);
    assert.deepEqual(s[2], {t: 400, bytes: 300});
    // 非法输入安全
    assert.deepEqual(pushProgressSample(null, 0, 0), [{t: 0, bytes: 0}]);

    // 窗口上限：超过 12 条淘汰最旧
    let big = [];
    for (let i = 0; i < 20; i++) big = pushProgressSample(big, i * 200, i * 1000);
    assert.equal(big.length, 12);
    assert.equal(big[0].t, 8 * 200, "最旧样本被淘汰");
}

// ── pushProgressSample：字节回退（多文件安装切换）清空窗口 ───────────────────

{
    let s = pushProgressSample([{t: 0, bytes: 5000}, {t: 200, bytes: 8000}], 400, 100);
    assert.deepEqual(s, [{t: 400, bytes: 100}], "回退时窗口重置");
    // 持平不清空（网络停滞 bytes 不变）
    s = pushProgressSample([{t: 0, bytes: 5000}], 200, 5000);
    assert.equal(s.length, 2);
}

// ── estimateEtaMs ─────────────────────────────────────────────────────────────

{
    const total = 10_000_000;
    // 样本不足
    assert.equal(estimateEtaMs([], total), null);
    assert.equal(estimateEtaMs([{t: 0, bytes: 0}], total), null);
    // 总量未知 / 非法
    assert.equal(estimateEtaMs([{t: 0, bytes: 0}, {t: 1000, bytes: 100}], null), null);
    assert.equal(estimateEtaMs([{t: 0, bytes: 0}, {t: 1000, bytes: 100}], -1), null);
    // 跨度太短（<800ms）
    assert.equal(estimateEtaMs([{t: 0, bytes: 0}, {t: 500, bytes: 1000}], total), null);
    // 正常估算：1s 内下了 1MB，剩 9MB → 9000ms
    const eta = estimateEtaMs([{t: 0, bytes: 0}, {t: 1000, bytes: 1_000_000}], total);
    assert.equal(eta, 9000);
    // 速度为零（停滞）不可估
    assert.equal(estimateEtaMs([{t: 0, bytes: 500}, {t: 1000, bytes: 500}], total), null);
    // 已下完 → 0
    assert.equal(estimateEtaMs([{t: 0, bytes: 0}, {t: 1000, bytes: total}], total), 0);
    // 极慢网络（ETA > 30 分钟）不可信 → null
    const huge = 100_000_000_000;
    const slow = estimateEtaMs([{t: 0, bytes: 0}, {t: 1000, bytes: 100}], huge);
    assert.equal(slow, null);
    // 非法输入
    assert.equal(estimateEtaMs(null, total), null);
}

// ── etaTextKeyAndParams：分桶 ─────────────────────────────────────────────────

{
    assert.equal(etaTextKeyAndParams(-1), null);
    assert.equal(etaTextKeyAndParams(Number.NaN), null);
    assert.deepEqual(etaTextKeyAndParams(0), {key: "local_engine.progress.eta.sec", params: {sec: 1}});
    assert.deepEqual(etaTextKeyAndParams(59_000), {key: "local_engine.progress.eta.sec", params: {sec: 59}});
    assert.deepEqual(etaTextKeyAndParams(60_000), {key: "local_engine.progress.eta.min", params: {min: 1}});
    assert.deepEqual(etaTextKeyAndParams(90_000), {
        key: "local_engine.progress.eta.min",
        params: {min: 2}
    }, "秒数向上取整到分钟");
    assert.deepEqual(etaTextKeyAndParams(3_600_000), {key: "local_engine.progress.eta.hour", params: {hour: 1}});
}

// ── formatBytes ───────────────────────────────────────────────────────────────

{
    assert.equal(formatBytes(0), "0 B");
    assert.equal(formatBytes(-5), "0 B");
    assert.equal(formatBytes(Number.NaN), "0 B");
    assert.equal(formatBytes(512), "512 B");
    assert.equal(formatBytes(1024), "1.0 KB");
    assert.equal(formatBytes(1536), "1.5 KB");
    assert.equal(formatBytes(200 * 1024), "200 KB");
    assert.equal(formatBytes(17.4 * 1024 * 1024), "17.4 MB");
    assert.equal(formatBytes(1.5 * 1024 * 1024 * 1024), "1.5 GB");
}

// ── progressPercent ───────────────────────────────────────────────────────────

{
    assert.equal(progressPercent(50, 0), null, "总量未知");
    assert.equal(progressPercent(50, null), null);
    assert.equal(progressPercent(50, -1), null);
    assert.equal(progressPercent(0, 100), 0);
    assert.equal(progressPercent(50, 200), 25);
    assert.equal(progressPercent(200, 200), 100);
    assert.equal(progressPercent(250, 200), 100, "钳制到 100");
    assert.equal(progressPercent(-10, 200), 0);
}

console.log("shared/download-progress.test.mjs: all assertions passed");
