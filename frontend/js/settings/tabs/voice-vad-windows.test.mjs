// 0.23.7 VAD 窗口高级配置测试：纯逻辑归一化 + 界面/词条静态对齐
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";

import {VAD_DEFAULTS, VAD_WINDOW_KEYS, VAD_WINDOW_RANGE, ensureVadWindowFields, normalizeVadWindows,} from "./voice-vad.js";

// ── 默认值与 Rust 侧对齐（8/12/12 秒）──

assert.deepEqual(
    {
        soft_window_s: VAD_DEFAULTS.soft_window_s,
        hard_window_s: VAD_DEFAULTS.hard_window_s,
        max_uncommitted_s: VAD_DEFAULTS.max_uncommitted_s,
    },
    {soft_window_s: 8, hard_window_s: 12, max_uncommitted_s: 12},
);

// ── normalizeVadWindows：合法组合不动点 ──

{
    const vad = {...VAD_DEFAULTS};
    assert.equal(normalizeVadWindows(vad), false, "默认值 8/12/12 不应被调整");
    assert.deepEqual(
        [vad.soft_window_s, vad.hard_window_s, vad.max_uncommitted_s],
        [8, 12, 12],
    );
}

{
    // H 组合 10/14/16：soft < hard <= uncommitted，合法
    const vad = {soft_window_s: 10, hard_window_s: 14, max_uncommitted_s: 16};
    assert.equal(normalizeVadWindows(vad), false);
    assert.deepEqual([vad.soft_window_s, vad.hard_window_s, vad.max_uncommitted_s], [10, 14, 16]);
}

// ── normalizeVadWindows：非法组合安全归一化（与 Rust sanitize 一致）──

{
    // 顺序破坏：soft >= hard >= uncommitted
    const vad = {soft_window_s: 15, hard_window_s: 10, max_uncommitted_s: 8};
    assert.equal(normalizeVadWindows(vad), true);
    assert.deepEqual([vad.soft_window_s, vad.hard_window_s, vad.max_uncommitted_s], [7, 8, 8]);
    assert.ok(vad.soft_window_s < vad.hard_window_s);
    assert.ok(vad.hard_window_s <= vad.max_uncommitted_s);
}

{
    // 越界上限：clamp 后按顺序约束前移
    const vad = {soft_window_s: 100, hard_window_s: 200, max_uncommitted_s: 999};
    normalizeVadWindows(vad);
    assert.deepEqual(
        [vad.soft_window_s, vad.hard_window_s, vad.max_uncommitted_s],
        [VAD_WINDOW_RANGE.soft_window_s.max - 1, VAD_WINDOW_RANGE.hard_window_s.max, VAD_WINDOW_RANGE.max_uncommitted_s.max],
    );
}

{
    // 越界下限（损坏值 0）
    const vad = {soft_window_s: 0, hard_window_s: 0, max_uncommitted_s: 0};
    normalizeVadWindows(vad);
    assert.ok(vad.soft_window_s >= VAD_WINDOW_RANGE.soft_window_s.min);
    assert.ok(vad.hard_window_s >= VAD_WINDOW_RANGE.hard_window_s.min);
    assert.ok(vad.soft_window_s < vad.hard_window_s);
    assert.ok(vad.hard_window_s <= vad.max_uncommitted_s);
}

{
    // 相等值：soft == hard == uncommitted（全 12）→ soft 前移
    const vad = {soft_window_s: 12, hard_window_s: 12, max_uncommitted_s: 12};
    assert.equal(normalizeVadWindows(vad), true);
    assert.deepEqual([vad.soft_window_s, vad.hard_window_s, vad.max_uncommitted_s], [11, 12, 12]);
}

// ── ensureVadWindowFields：旧配置缺字段回填默认并归一化 ──

{
    const vad = {silence_threshold: 0.005, min_silence_ms: 300, min_sentence_ms: 1000};
    assert.equal(ensureVadWindowFields(vad), false, "回填默认值不算调整");
    assert.deepEqual(
        [vad.soft_window_s, vad.hard_window_s, vad.max_uncommitted_s],
        [8, 12, 12],
        "旧配置缺窗口字段时保持 8/12/12 既有行为",
    );
}

{
    const vad = {soft_window_s: "bad", hard_window_s: 12, max_uncommitted_s: 12};
    ensureVadWindowFields(vad);
    assert.equal(vad.soft_window_s, 8, "非法旧值回填默认后仍满足顺序约束");
    assert.ok(vad.soft_window_s < vad.hard_window_s);
}

// ── 界面静态对齐：三个滑块 + 分组标题 + 可访问名称 ──

const voiceSource = await readFile(new URL("./voice.js", import.meta.url), "utf8");
assert.doesNotMatch(voiceSource, /__TAURI__/);
for (const id of [
    "voice-vad-soft-window-s",
    "voice-vad-hard-window-s",
    "voice-vad-max-uncommitted-s",
]) {
    assert.match(voiceSource, new RegExp(`getElementById\\("${id}"\\)`), `voice.js 应接线 ${id}`);
}
// 窗口滑块 change 时必须联动归一化
assert.match(voiceSource, /isWindow[\s\S]*?normalizeVadWindows\(vad\)/);
// 恢复默认必须覆盖全部 6 个参数
assert.match(voiceSource, /vad\[control\.key\] = VAD_DEFAULTS\[control\.key\]/);
// 滑块需要可访问名称（i18n 驱动）
assert.match(voiceSource, /setAttribute\("aria-label", t\(control\.ariaKey\)\)/);

const html = await readFile(new URL("../../../settings.html", import.meta.url), "utf8");
for (const id of [
    "voice-vad-soft-window-s",
    "voice-vad-soft-window-s-val",
    "voice-vad-hard-window-s",
    "voice-vad-hard-window-s-val",
    "voice-vad-max-uncommitted-s",
    "voice-vad-max-uncommitted-s-val",
]) {
    assert.match(html, new RegExp(`id="${id}"`), `settings.html 应包含 ${id}`);
}
// 滑块范围与安全边界一致
assert.match(html, /id="voice-vad-soft-window-s" max="20"\s*min="3"/);
assert.match(html, /id="voice-vad-hard-window-s" max="30"\s*min="5"/);
assert.match(html, /id="voice-vad-max-uncommitted-s" max="30"\s*min="5"/);

// ── i18n：zh/en 词条 key 对齐 ──

const VAD_I18N_KEYS = [
    "voice.local.vad.windows.title",
    "voice.local.vad.soft_window_s.label",
    "voice.local.vad.soft_window_s.hint",
    "voice.local.vad.hard_window_s.label",
    "voice.local.vad.hard_window_s.hint",
    "voice.local.vad.max_uncommitted_s.label",
    "voice.local.vad.max_uncommitted_s.hint",
];
for (const lang of ["zh", "en"]) {
    const source = await readFile(new URL(`../../i18n/${lang}.js`, import.meta.url), "utf8");
    for (const key of VAD_I18N_KEYS) {
        assert.match(source, new RegExp(`"${key}":`), `${lang}.js 缺少词条 ${key}`);
    }
}

// ── 防回退：窗口参数不得再回到硬编码 ──

assert.equal(VAD_WINDOW_KEYS.length, 3);

console.log("voice-vad-windows.test.mjs: all assertions passed");
