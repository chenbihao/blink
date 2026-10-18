/**
 * 快捷超链 Tab 纯函数回归（0.26.12）。
 * 运行：node frontend/js/settings/tabs/weblinks.test.mjs
 */
import assert from 'node:assert/strict';

// weblinks.js 顶层 import tauri.js/i18n——其模块级代码访问 window/document，
// Node 测试环境先补最小全局（同 list-navigation.test.mjs 模式）。
globalThis.window = globalThis;
globalThis.document = globalThis.document ?? {
    getElementById: () => null,
    createElement: () => ({classList: {add() {}, remove() {}, toggle() {}}, append() {}, appendChild() {}}),
};

const {isSearchTemplate, normalizeConfig, localValidate, parseConflictError, describeConflicts} =
    await import('./weblinks.js');

// ── isSearchTemplate ────────────────────────────────────────────────────────
assert.equal(isSearchTemplate('https://a.test/s?q={query}'), true);
assert.equal(isSearchTemplate('https://a.test'), false);
assert.equal(isSearchTemplate(null), false);

// ── normalizeConfig ─────────────────────────────────────────────────────────
const norm = normalizeConfig({entries: [
    {id: 'g', name: 'Google', keyword: 'g', url_template: 'https://g.test/?q={query}'},
    {id: 'gh', name: 'GitHub', keyword: 'gh', url_template: 'https://gh.test', enabled: false},
    null,
    {name: '无 id 兜底'},
]});
assert.equal(norm.seeded, true);
assert.equal(norm.entries.length, 3);
assert.equal(norm.entries[0].enabled, true, '缺省 enabled=true');
assert.equal(norm.entries[1].enabled, false);
assert.ok(norm.entries[2].id.startsWith('entry-'), '无 id 生成兜底 id');
assert.deepEqual(normalizeConfig(null), {entries: [], seeded: true});
assert.deepEqual(normalizeConfig({no: 'entries'}), {entries: [], seeded: true});
assert.equal(normalizeConfig({entries: [], seeded: false}).seeded, false);

// ── localValidate ───────────────────────────────────────────────────────────
assert.deepEqual(localValidate({name: 'A', keyword: 'ok', url_template: 'https://a.test'}), []);
assert.deepEqual(localValidate({name: ' ', keyword: 'ok', url_template: 'https://a.test'}), ['name_empty']);
assert.deepEqual(localValidate({name: 'A', keyword: 'go od', url_template: 'https://a.test'}), ['keyword_invalid']);
assert.deepEqual(localValidate({name: 'A', keyword: '  ', url_template: 'https://a.test'}), ['keyword_invalid']);
assert.deepEqual(localValidate({name: 'A', keyword: 'AI', url_template: 'https://a.test'}), ['keyword_reserved']);
assert.deepEqual(localValidate({name: 'A', keyword: 'ok', url_template: 'ftp://a.test'}), ['url_invalid']);
assert.deepEqual(localValidate({name: 'A', keyword: 'ok', url_template: 'a.test'}), ['url_invalid']);
// 多 issue 同时返回
assert.deepEqual(
    localValidate({name: '', keyword: 'ai', url_template: 'x'}),
    ['name_empty', 'keyword_reserved', 'url_invalid'],
);

// ── parseConflictError ──────────────────────────────────────────────────────
const conflicts = [
    {entry_id: 'x1', keyword: 'clip', kind: 'taken_by_engine', owner: 'clipboard'},
    {entry_id: 'x2', keyword: 'dup', kind: 'duplicate'},
];
const parsed = parseConflictError({
    message: 'keyword_conflict: ' + JSON.stringify(conflicts),
});
assert.deepEqual(parsed, conflicts);
assert.equal(parseConflictError({message: 'config_update_failed: boom'}), null);
assert.deepEqual(parseConflictError({message: 'keyword_conflict: not-json'}), [],
    '损坏 JSON 返回空数组（有冲突但详情不可解析），区别于 null（非冲突错误）');
assert.equal(parseConflictError('keyword_conflict: []').length, 0);

// ── describeConflicts ───────────────────────────────────────────────────────
const dict = {
    'weblinks.conflict.taken_by_engine': '已被引擎占用',
    'weblinks.conflict.duplicate': '表内重复',
};
const lines = describeConflicts(conflicts, (k) => dict[k] ?? k);
assert.deepEqual(lines, ['clip: 已被引擎占用 (clipboard)', 'dup: 表内重复']);

console.log('weblinks.test.mjs: all assertions passed');
