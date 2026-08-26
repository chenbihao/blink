/**
 * 本地引擎 controller 竞态测试（0.22.5 H1）。
 *
 * 测试覆盖：
 * 1. mount 期间事件进入缓冲，mount 完成后 flush
 * 2. dispose 后不再处理事件
 * 3. dispose 期间异步 mount 完成时不激活（mount generation 失效）
 * 4. 重复 mount 不创建重复监听
 * 5. registerListeners 失败时回滚已注册 listener
 * 6. refreshStatus 期间事件进入缓冲
 * 7. 事件 payload 直接作为 EngineStatusDto 合并（不再从 payload.snapshot 提取）
 *
 * 通过注入 globalThis.window.__TAURI__ mock invoke/listen。
 */

import assert from "node:assert/strict";

// ── 设置 globalThis.window（tauri.js 在模块加载时访问 window.__TAURI__）─────────
// 必须在导入 local-runtime.js 之前设置，否则 tauri.js 会抛 ReferenceError。
// tauri.js 在加载时执行 const TAU = window.__TAURI__，
// export const invoke = TAU?.core?.invoke（绑定到 TAU.core.invoke 的函数引用），
// 所以必须在加载前就设置好 __TAURI__，且 invoke 用闭包代理使其可运行时替换。
if (!globalThis.window) {
    globalThis.window = {};
}

// 闭包代理——后续 createMockEnvironment 会替换 invokeImpl/listenImpl
const _invokeImpl = {fn: async () => []};
const _listenImpl = {fn: async () => () => {}};
globalThis.window.__TAURI__ = {
    core: {
        // invoke 绑定到箭头函数，实际调用 _invokeImpl.fn（可运行时替换）
        invoke: (cmd, args) => _invokeImpl.fn(cmd, args),
    },
    event: {
        // listen 同样用闭包代理
        listen: (event, handler) => _listenImpl.fn(event, handler),
    },
};

// ── mock 基础设施 ───────────────────────────────────────────────────────────

/**
 * 创建 mock 的 invoke/listen 注入环境。
 *
 * tauri.js 在模块加载时绑定了 invoke = TAU.core.invoke 和 listen 函数（检查 TAU.event.listen）。
 * 由于 TAU 在加载时引用了 window.__TAURI__，我们通过替换 TAU 的内部属性来 mock：
 * - invoke 是箭头函数 (cmd, args) => _invokeImpl.fn(cmd, args)，替换 _invokeImpl.fn 即可
 * - listen 是函数 listen(event, handler) { if (TAU?.event?.listen) ... }，替换 TAU.event.listen 即可
 */
function createMockEnvironment() {
    const listeners = new Map(); // event name → handler

    // 替换 invoke 闭包代理的实现
    _invokeImpl.fn = async () => [];

    // 替换 listen 实现（通过 TAU.event.listen）
    const mockListen = async (event, handler) => {
        if (!listeners.has(event)) {
            listeners.set(event, []);
        }
        listeners.get(event).push(handler);
        return () => {
            const arr = listeners.get(event);
            if (arr) {
                const idx = arr.indexOf(handler);
                if (idx >= 0) arr.splice(idx, 1);
            }
        };
    };
    // TAU.event 引用的是 globalThis.window.__TAURI__.event
    // 但 tauri.js 的 listen 函数检查的是 TAU?.event?.listen
    // 由于 TAU 在加载时引用了 window.__TAURI__，我们需要修改 TAU.event.listen
    // 但 TAU 不是直接可访问的——我们需要通过 window.__TAURI__ 间接修改
    globalThis.window.__TAURI__.event.listen = mockListen;

    let invokeImpl = async () => [];
    const setInvoke = (impl) => { invokeImpl = impl; _invokeImpl.fn = impl; };

    return {
        listeners,
        setInvoke,
        emit(event, payload) {
            const handlers = listeners.get(event) || [];
            for (const h of handlers) {
                h({payload});
            }
        },
        cleanup() {
            listeners.clear();
            // 恢复默认
            _invokeImpl.fn = async () => [];
            globalThis.window.__TAURI__.event.listen = _listenImpl.fn;
        },
    };
}

// ── 动态导入 controller（需要在 mock 设置后）────────────────────────────────

let testCount = 0;
let passCount = 0;

async function test(name, fn) {
    testCount++;
    try {
        await fn();
        passCount++;
        console.log(`  ✓ ${name}`);
    } catch (e) {
        console.error(`  ✗ ${name}`);
        console.error(`    ${e.message}`);
        console.error(`    ${e.stack?.split("\n")[1]?.trim() || ""}`);
        throw e;
    }
}

// ── 测试主体 ────────────────────────────────────────────────────────────────

async function runTests() {
    const {createLocalEngineController} = await import("./local-runtime.js");
    const {makeCatalog, makeStatus, makeLog, processState} = await import("./local-engine-fixtures.js");

    // ── 1. mount 期间事件进入缓冲，mount 完成后 flush ────────────────────────

    await test("mount 期间事件进入缓冲，mount 完成后 flush", async () => {
        const env = createMockEnvironment();
        env.setInvoke(async (cmd) => {
            if (cmd === "get_local_engine_catalog") return makeCatalog();
            if (cmd === "get_local_engine_status") return [];
            if (cmd === "get_local_engine_logs") return [];
            if (cmd === "get_local_engine_storage") return {engine_id: "funasr", targets: [], total_size_bytes: 0, releasable_size_bytes: 0};
            return [];
        });

        const controller = createLocalEngineController({});

        // 在 mount 过程中发出一个 status 事件（应该被缓冲）
        const statusEvent = makeStatus({
            engine_id: "funasr",
            service_epoch: "epoch-test1",
            revision: "1",
            status: {environment: "ready", process: processState.running(1234)},
        });

        // mount 开始 → buffering=true
        const mountPromise = controller.mount();

        // 在 mount 进行中发出事件（应进入缓冲）
        env.emit("blink://local-engine-status", statusEvent);

        await mountPromise;

        // mount 完成后缓冲应被 flush
        const state = controller.getState();
        const entry = state.get("funasr");
        assert.ok(entry, "funasr entry 应存在");
        assert.ok(entry.status, "status 应存在");
        assert.equal(entry.status.service_epoch, "epoch-test1");

        controller.dispose();
        env.cleanup();
    });

    // ── 2. dispose 后不再处理事件 ────────────────────────────────────────────

    await test("dispose 后不再处理事件", async () => {
        const env = createMockEnvironment();
        env.setInvoke(async (cmd) => {
            if (cmd === "get_local_engine_catalog") return makeCatalog();
            return [];
        });

        let stateChangeCount = 0;
        const controller = createLocalEngineController({
            onStateChange: () => { stateChangeCount++; },
        });

        await controller.mount();
        const changesBefore = stateChangeCount;

        controller.dispose();

        // dispose 后发出事件 → 不应触发 stateChange
        env.emit("blink://local-engine-status", makeStatus({
            engine_id: "funasr",
            service_epoch: "epoch-after-dispose",
            revision: "99",
        }));

        assert.equal(stateChangeCount, changesBefore, "dispose 后不应处理事件");

        env.cleanup();
    });

    // ── 3. dispose 期间异步 mount 完成时不激活 ────────────────────────────────

    await test("dispose 期间异步 mount 完成时不激活（mount generation 失效）", async () => {
        const env = createMockEnvironment();
        let catalogResolve;
        env.setInvoke(async (cmd) => {
            if (cmd === "get_local_engine_catalog") {
                return new Promise((resolve) => {
                    catalogResolve = () => resolve(makeCatalog());
                });
            }
            return [];
        });

        const controller = createLocalEngineController({});

        // 开始 mount（会卡在 catalog pull）
        const mountPromise = controller.mount();

        // 等一个 microtask 让 mount 进入 pullCatalog 的 await
        await new Promise((r) => setTimeout(r, 10));

        // 在 mount 进行中 dispose
        controller.dispose();

        // 完成 catalog pull
        assert.ok(typeof catalogResolve === "function", "catalogResolve 应已被赋值");
        catalogResolve();

        // mount 应完成但不激活
        await mountPromise;

        assert.equal(controller.isMounted(), false, "dispose 后 mount 不应激活");
        assert.equal(controller.isDisposed(), true);

        env.cleanup();
    });

    // ── 4. 重复 mount 不创建重复监听 ──────────────────────────────────────────

    await test("重复 mount 不创建重复监听", async () => {
        const env = createMockEnvironment();
        env.setInvoke(async (cmd) => {
            if (cmd === "get_local_engine_catalog") return makeCatalog();
            return [];
        });

        const controller = createLocalEngineController({});
        await controller.mount();
        await controller.mount(); // 幂等

        // 验证 listener 没有重复
        const statusHandlers = env.listeners.get("blink://local-engine-status") || [];
        assert.equal(statusHandlers.length, 1, "不应有重复 status listener");

        const logHandlers = env.listeners.get("blink://local-engine-log") || [];
        assert.equal(logHandlers.length, 1, "不应有重复 log listener");

        controller.dispose();
        env.cleanup();
    });

    // ── 5. registerListeners 失败时回滚 ────────────────────────────────────────

    await test("registerListeners 失败时回滚已注册 listener", async () => {
        const env = createMockEnvironment();
        // 让第二个 listen 调用失败
        let listenCallCount = 0;
        const originalListen = globalThis.window.__TAURI__.event.listen;
        globalThis.window.__TAURI__.event.listen = async (event, handler) => {
            listenCallCount++;
            if (listenCallCount === 2) {
                throw new Error("mock listen failure");
            }
            return originalListen(event, handler);
        };

        env.setInvoke(async (cmd) => {
            if (cmd === "get_local_engine_catalog") return makeCatalog();
            return [];
        });

        const controller = createLocalEngineController({});

        let mountError = null;
        try {
            await controller.mount();
        } catch (e) {
            mountError = e;
        }

        assert.ok(mountError, "mount 应因 listen 失败而抛错");
        assert.ok(mountError.message.includes("mock listen failure") || mountError.message.includes("listen"), "错误信息应包含 listen 失败");

        // 第一个 listener 应被回滚（清理）
        const statusHandlers = env.listeners.get("blink://local-engine-status") || [];
        assert.equal(statusHandlers.length, 0, "失败的 mount 应回滚已注册 listener");

        controller.dispose();
        // 恢复原始 listen
        globalThis.window.__TAURI__.event.listen = originalListen;
        env.cleanup();
    });

    // ── 6. refreshStatus 期间事件进入缓冲 ──────────────────────────────────────

    await test("refreshStatus 期间事件进入缓冲", async () => {
        const env = createMockEnvironment();
        env.setInvoke(async (cmd) => {
            if (cmd === "get_local_engine_catalog") return makeCatalog();
            if (cmd === "get_local_engine_status") return [];
            if (cmd === "get_local_engine_logs") return [];
            return [];
        });

        const controller = createLocalEngineController({});
        await controller.mount();

        // 初始状态
        let state = controller.getState();
        assert.ok(!state.get("funasr")?.status, "初始不应有 status");

        // refreshStatus 开始 → buffering=true
        const refreshPromise = controller.refreshStatus();

        // 在 refresh 进行中发出事件（应进入缓冲）
        const statusEvent = makeStatus({
            engine_id: "funasr",
            service_epoch: "epoch-refresh",
            revision: "1",
            status: {environment: "ready"},
        });
        env.emit("blink://local-engine-status", statusEvent);

        await refreshPromise;

        // refresh 完成后缓冲应被 flush
        state = controller.getState();
        const entry = state.get("funasr");
        assert.ok(entry?.status, "refresh 后应有 status");
        assert.equal(entry.status.service_epoch, "epoch-refresh");

        controller.dispose();
        env.cleanup();
    });

    // ── 7. 事件 payload 直接作为 EngineStatusDto 合并 ────────────────────────────

    await test("事件 payload 直接作为 EngineStatusDto 合并（不再从 payload.snapshot 提取）", async () => {
        const env = createMockEnvironment();
        env.setInvoke(async (cmd) => {
            if (cmd === "get_local_engine_catalog") return makeCatalog();
            return [];
        });

        const controller = createLocalEngineController({});
        await controller.mount();

        // 发出一个 status 事件——payload 是 EngineStatusDto
        // （engine_id + service_epoch + revision + status 在顶层，不在 snapshot 子对象中）
        const statusPayload = makeStatus({
            engine_id: "funasr",
            service_epoch: "epoch-direct",
            revision: "1",
            status: {
                environment: "ready",
                process: processState.running(7777),
            },
        });

        env.emit("blink://local-engine-status", statusPayload);

        const state = controller.getState();
        const entry = state.get("funasr");
        assert.ok(entry?.status, "应有 status");
        assert.equal(entry.status.service_epoch, "epoch-direct");
        assert.equal(entry.status.engine_id, "funasr");
        // status 字段直接在顶层，不是从 snapshot 提取
        assert.equal(entry.status.status.environment, "ready");
        assert.equal(entry.status.status.process.state, "running");
        assert.equal(entry.status.status.process.pid, 7777);

        controller.dispose();
        env.cleanup();
    });

    // ── 8. ProcessStateDto shape 事件正确处理 ───────────────────────────────────

    await test("ProcessStateDto shape 事件正确处理（stopped 字符串不抛异常）", async () => {
        const env = createMockEnvironment();
        env.setInvoke(async (cmd) => {
            if (cmd === "get_local_engine_catalog") return makeCatalog();
            return [];
        });

        const controller = createLocalEngineController({});
        await controller.mount();

        // 发出 stopped 状态——ProcessStateDto 是对象 {state: "stopped"}
        // 旧 shape "stopped" 是字符串，in 操作符会抛 TypeError
        const stoppedStatus = makeStatus({
            engine_id: "funasr",
            service_epoch: "epoch-stopped",
            revision: "1",
            status: {
                environment: "ready",
                process: processState.stopped(),
            },
        });

        // 不应抛异常
        env.emit("blink://local-engine-status", stoppedStatus);

        const state = controller.getState();
        const entry = state.get("funasr");
        assert.equal(entry.status.status.process.state, "stopped");

        controller.dispose();
        env.cleanup();
    });
}

// ── 运行 ────────────────────────────────────────────────────────────────────

runTests()
    .then(() => {
        console.log(`\n${passCount}/${testCount} tests passed`);
        if (passCount !== testCount) {
            process.exit(1);
        }
        console.log("local-engine-controller tests passed");
    })
    .catch((e) => {
        console.error(`\n${passCount}/${testCount} tests passed`);
        console.error("Fatal:", e);
        process.exit(1);
    });
