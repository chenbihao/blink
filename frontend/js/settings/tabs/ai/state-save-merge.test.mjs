import {test} from 'node:test';
import assert from 'node:assert/strict';

// Node 环境最小 DOM/桥接替身（settings 各 tab 测试同款模式）。
const savedPayloads = [];
let backendConfig = {providers: [{id: 'a', name: 'A'}], enabled: true, tier_light: null};

globalThis.window = {
    __TAURI__: {
        core: {
            invoke: async (command, args) => {
                if (command === 'get_config_section') {
                    return structuredClone(backendConfig);
                }
                if (command === 'set_config') {
                    savedPayloads.push(structuredClone(args.value));
                    backendConfig = structuredClone(args.value);
                    return null;
                }
                return null;
            },
        },
    },
};
globalThis.document = {getElementById: () => null};

const {aiState, saveAIConfig, snapshotLoadedAIConfig} = await import('./state.js');

test('0.28.28：保存合并——本页未触碰的供应商不被旧对象覆盖', async () => {
    // 页面加载到 A（快照基线）
    const loaded = {providers: [{id: 'a', name: 'A'}], enabled: true, tier_light: null};
    aiState.currentAIConfig = structuredClone(loaded);
    snapshotLoadedAIConfig(structuredClone(loaded));

    // 同步在页面背后新增供应商 b（后端最新）
    backendConfig = {
        providers: [{id: 'a', name: 'A'}, {id: 'b', name: 'B-synced'}],
        enabled: true,
        tier_light: null,
    };

    // 用户只改了 enabled（本页改动字段）
    aiState.currentAIConfig.enabled = false;
    await saveAIConfig();

    assert.equal(savedPayloads.length, 1);
    const payload = savedPayloads[0];
    assert.equal(payload.enabled, false, '本页改动生效');
    assert.deepEqual(
        payload.providers,
        [{id: 'a', name: 'A'}, {id: 'b', name: 'B-synced'}],
        '刚同步的供应商不被旧对象覆盖（字段补丁合并）',
    );
});

test('0.28.28：本页改动的顶层字段整字段生效，未改字段取后端权威值', async () => {
    savedPayloads.length = 0;
    const loaded = {providers: [{id: 'a'}], enabled: false, tier_light: null, tier_main: null, slo: 1};
    aiState.currentAIConfig = structuredClone(loaded);
    snapshotLoadedAIConfig(structuredClone(loaded));

    // 后端权威值变化（另一窗口改 tier_main）
    backendConfig = {providers: [{id: 'a'}], enabled: false, tier_light: null, tier_main: {provider_id: 'p', model_id: 'm'}, slo: 1};
    // 本页改 tier_light
    aiState.currentAIConfig.tier_light = {provider_id: 'p', model_id: 'l'};
    await saveAIConfig();

    const payload = savedPayloads[0];
    assert.deepEqual(payload.tier_light, {provider_id: 'p', model_id: 'l'}, '本页改动生效');
    assert.deepEqual(payload.tier_main, {provider_id: 'p', model_id: 'm'}, '未触碰字段取后端权威值');
    // 收敛：本页状态 = 已保存载荷（mock 里 savedPayloads 是深拷贝，按内容比对）
    assert.deepEqual(aiState.currentAIConfig, savedPayloads[0]);
});

test('0.28.28：本地无改动时保存幂等（不把旧列表写回去）', async () => {
    savedPayloads.length = 0;
    const loaded = {providers: [{id: 'a'}], enabled: false};
    aiState.currentAIConfig = structuredClone(loaded);
    snapshotLoadedAIConfig(structuredClone(loaded));

    // 后端被同步更新，本页未做任何编辑
    backendConfig = {providers: [{id: 'a'}, {id: 'c'}], enabled: false};
    await saveAIConfig();

    assert.equal(savedPayloads.length, 1);
    assert.deepEqual(savedPayloads[0].providers, [{id: 'a'}, {id: 'c'}], '幂等保存取后端最新值');
});

test('0.28.28：保存串行化——并发保存不互相读到中间态', async () => {
    savedPayloads.length = 0;
    const loaded = {providers: [{id: 'a'}], enabled: false, x: 0};
    aiState.currentAIConfig = structuredClone(loaded);
    snapshotLoadedAIConfig(structuredClone(loaded));

    // 模拟两次快速编辑 + 两次保存（第二次编辑在第一次保存进行中发生）
    aiState.currentAIConfig.enabled = true;
    const p1 = saveAIConfig();
    aiState.currentAIConfig.x = 42;
    const p2 = saveAIConfig();
    await Promise.all([p1, p2]);

    assert.equal(savedPayloads.length, 2);
    assert.equal(savedPayloads[1].x, 42, '第二次保存携带第二次编辑');
    assert.equal(savedPayloads[1].enabled, true, '第一次编辑保留');
});
