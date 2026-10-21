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

const {visibleEntries, entryFromDraft, inferHomepage, isSearchTemplate, normalizeConfig, localValidate, parseConflictError, describeConflicts,
    normalizeCandidates, importKeywordIssue, buildImportEntries, applySelectAll, invertSelection, selectAllState} =
    await import('./weblinks.js');

// ── isSearchTemplate ────────────────────────────────────────────────────────
assert.equal(isSearchTemplate('https://a.test/s?q={query}'), true);
assert.equal(isSearchTemplate('https://a.test'), false);
assert.equal(isSearchTemplate(null), false);

assert.equal(inferHomepage('https://sub.example.co.uk:8443/find?q={query}#results'), 'https://sub.example.co.uk:8443/');
assert.equal(inferHomepage('https://example.com/projects?q=new#top'), 'https://example.com/projects?q=new#top');
for (const invalid of ['https://', 'javascript:alert(1)', 'https://user:pass@example.com']) assert.equal(inferHomepage(invalid), '');
assert.deepEqual(localValidate({name:'Bili',keyword:'bili',home_url:'https://www.bilibili.com/',url_template:'https://search.bilibili.com/all?keyword={query}'}), []);
assert.deepEqual(localValidate({name:'Home',keyword:'home',home_url:'https://example.com/portal',url_template:''}), []);
assert.deepEqual(localValidate({name:'Home',keyword:'home',home_url:'https://example.com/{query}',url_template:''}), ['url_invalid']);
assert.deepEqual(localValidate({name:'Home',keyword:'home',home_url:'https://example.com/',url_template:'https://example.com/search'}), ['search_query_missing']);

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
assert.equal(norm.entries[0].home_url, '', '旧条目默认自动提取主页');
assert.equal(normalizeConfig({entries:[{home_url:'https://example.com/portal'}]}).entries[0].home_url, 'https://example.com/portal');
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

// 编辑已禁用的条目，不得悄悄启用；新条目默认启用。
const changed = {id: 'off', name: 'Renamed', keyword: 'off', url_template: 'https://new.test'};
assert.equal(entryFromDraft({enabled: false}, changed).enabled, false);
assert.equal(entryFromDraft({enabled: true}, changed).enabled, true);
assert.equal(entryFromDraft(null, changed).enabled, true);

// 长列表视图：字母/拼音排序、自然数字、同名稳定排序、筛选不改存储顺序。
const entries = [
    {id:'z', name:'知乎', keyword:'zh', home_url:'https://zhihu.com/'},
    {id:'g', name:'GitHub', keyword:'gh', url_template:'https://github.com/search?q={query}'},
    {id:'b', name:'Bilibili', keyword:'bili'},
    {id:'10', name:'Site 10', keyword:'s10'},
    {id:'2', name:'Site 2', keyword:'s2'},
    {id:'bd', name:'百度', keyword:'bd'},
];
assert.deepEqual(visibleEntries(entries).map(e => e.id), ['b','g','2','10','bd','z']);
assert.deepEqual(entries.map(e => e.id), ['z','g','b','10','2','bd']);
assert.deepEqual(visibleEntries(entries, ' GH ').map(e => e.id), ['g']);
assert.deepEqual(visibleEntries(entries, 'ZHihU.com').map(e => e.id), ['z']);
assert.equal(visibleEntries(entries, 'absent').length, 0);

// ── 浏览器导入（0.26.21） ────────────────────────────────────────────────────

// normalizeCandidates：跳过项不勾选、可导入项默认勾选；坏输入防御。
const candidates = normalizeCandidates([
    {name: 'LINUX DO', keyword: 'linux.do', url_template: 'https://linux.do/search?q={query}'},
    {name: '书签', keyword: '@书签', url_template: 'chrome://bookmarks/?q={query}', skip: 'not_http'},
    null,
]);
assert.equal(candidates.length, 2);
assert.deepEqual(candidates[0], {name: 'LINUX DO', keyword: 'linux.do', urlTemplate: 'https://linux.do/search?q={query}', skip: null, checked: true});
assert.equal(candidates[1].checked, false, '跳过项默认不勾选');
assert.deepEqual(normalizeCandidates(null), []);
assert.deepEqual(normalizeCandidates('x'), []);

// importKeywordIssue：格式/保留前缀/表内重复/批内重复。
assert.equal(importKeywordIssue('ld', ['gh'], ['bili']), null);
assert.equal(importKeywordIssue('  ', [], []), 'keyword_invalid');
assert.equal(importKeywordIssue('go od', [], []), 'keyword_invalid');
assert.equal(importKeywordIssue('AI', [], []), 'keyword_reserved');
assert.equal(importKeywordIssue('GH', ['gh'], []), 'keyword_exists', '表内大小写不敏感');
assert.equal(importKeywordIssue('bili', [], ['BILI']), 'keyword_conflict_in_batch');

// buildImportEntries：只取勾选且校验通过的行；批内重复先到先得；条目形状对齐 WebLinkEntry。
const rows = normalizeCandidates([
    {name: 'LINUX DO', keyword: 'linux.do', url_template: 'https://linux.do/search?q={query}'},
    {name: '百度', keyword: 'Baidu', url_template: 'https://www.baidu.com/#ie=utf-8&wd={query}'},
    {name: '同词', keyword: 'linux.do', url_template: 'https://dup.test/?q={query}'},
    {name: '被占', keyword: 'gh', url_template: 'https://gh.taken/?q={query}'},
    {name: '书签', keyword: '@x', url_template: 'chrome://x', skip: 'not_http'},
]);
const built = buildImportEntries(rows, ['gh'], 1234);
assert.deepEqual(built.map(e => e.name), ['LINUX DO', '百度'], '勾选+通过才导入，批内重复与表内冲突排除');
assert.equal(built[0].keyword, 'linux.do');
assert.equal(built[0].home_url, '', '主页留空交后端 origin 推导');
assert.equal(built[0].url_template, 'https://linux.do/search?q={query}');
assert.equal(built[0].enabled, true);
assert.ok(built[0].id.startsWith('entry-1234-'));
// 取消勾选后不再导入；名称空时回退 keyword。
const unchecked = rows.map(r => ({...r, checked: r.name === '百度'}));
assert.deepEqual(buildImportEntries(unchecked, [], 99).map(e => e.name), ['百度']);
const anonymous = [{name: '', keyword: 'zz', urlTemplate: 'https://zz.test/?q={query}', skip: null, checked: true}];
assert.equal(buildImportEntries(anonymous, [], 1)[0].name, 'zz');

// 全选/反选与三态：只作用于可导入行，跳过行恒不勾选。
const mixed = [
    {name: 'A', keyword: 'a', urlTemplate: 'https://a.test/', skip: null, checked: true},
    {name: 'B', keyword: 'b', urlTemplate: 'https://b.test/', skip: null, checked: false},
    {name: 'X', keyword: 'x', urlTemplate: 'chrome://x', skip: 'not_http', checked: false},
];
assert.deepEqual(applySelectAll(mixed, true).map(r => r.checked), [true, true, false], '全选不碰跳过行');
assert.deepEqual(applySelectAll(mixed, false).map(r => r.checked), [false, false, false]);
assert.deepEqual(invertSelection(mixed).map(r => r.checked), [false, true, false], '反选仅翻转可导入行');
assert.deepEqual(selectAllState(mixed), {importable: 2, selected: 1, checked: false, indeterminate: true});
assert.deepEqual(selectAllState(applySelectAll(mixed, true)), {importable: 2, selected: 2, checked: true, indeterminate: false});
assert.deepEqual(selectAllState(applySelectAll(mixed, false)), {importable: 2, selected: 0, checked: false, indeterminate: false});
assert.equal(selectAllState([]).importable, 0, '空列表无可导入行');

console.log('weblinks.test.mjs: all assertions passed');
