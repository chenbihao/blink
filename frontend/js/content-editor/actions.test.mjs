/**
 * EditorActions（0.23.2）测试。
 *
 * 覆盖（phase 文档 §6.3）：
 * - targetDisplay：CommitTarget → 图标/i18n 键映射；
 * - pickSendText：选区优先、无选区回退全文；
 * - 次级动作不改主目标：save-copy 成功后 session.target 不变；
 * - saveTo/saveCopy：对话框取消 → 无 IPC 副作用；
 * - 冲突错误结构化透传给 onError；
 * - i18n 词条完整性（防字典漂移）。
 */

// tauri.js 顶层引用 window；renderTarget 测试用最小 document 桩
globalThis.window = globalThis;
globalThis.document = {
    createElement() {
        return {
            textContent: "",
            title: "",
            attributes: {},
            setAttribute(k, v) {
                this.attributes[k] = String(v);
            },
            getAttribute(k) {
                return this.attributes[k] ?? null;
            },
        };
    },
    createElementNS() {
        return this.createElement();
    },
};

const {test} = await import("node:test");
const assert = (await import("node:assert/strict")).default;
const {EditorActions, MENU_ITEMS, nextMenuIndex, targetDisplay, pickSendText, stickyConflictAction, fileConflictAction} = await import("./actions.js");
const {t: realT} = await import("../i18n/index.js");

function makeHarness() {
    const log = {commits: [], copies: [], stickies: [], chats: [], dialogs: [], status: [], errors: [], targetSaved: 0};
    const session = {
        isActive: true,
        target: {kind: "clipboard_result"},
        async commit(override) {
            log.commits.push(override);
            if (session._result) return session._result;
            return {ok: true};
        },
    };
    const adapter = {
        selection: "",
        getText: () => "全文内容",
        getSelectionText: () => adapter.selection,
    };
    const api = {
        async saveDialog(opts) {
            log.dialogs.push(opts);
            return api._dialogPath ?? null;
        },
        async copyToClipboard(text) {
            log.copies.push(text);
        },
        /** 0.23.7：create_sticky 走统一 Capability（创建 + 显示窗口同一原子入口） */
        async runBuiltinAction(id, arg) {
            if (id === "create_sticky") {
                log.stickies.push({content: arg?.content});
                if (api._stickyError) throw api._stickyError;
                return;
            }
            log.chats.push({id, arg});
        },
    };
    const actions = new EditorActions({
        session,
        adapter,
        api,
        callbacks: {
            onStatus: (m) => log.status.push(m),
            onError: (e) => log.errors.push(e),
            onTargetSaved: () => log.targetSaved += 1,
        },
    });
    return {actions, session, adapter, api, log};
}

test("targetDisplay maps commit target kinds", () => {
    assert.deepEqual(targetDisplay({kind: "clipboard_result"}), {icon: "copy", key: "editor.target.clipboard"});
    assert.deepEqual(targetDisplay({kind: "update_sticky", stickyId: "s1"}), {icon: "sticky-note", key: "editor.target.sticky"});
    assert.deepEqual(targetDisplay({kind: "confirmed_file", path: "D:\\a.md"}), {
        icon: "file-text",
        key: "editor.target.file",
        args: {path: "D:\\a.md"},
    });
    assert.deepEqual(targetDisplay({kind: "return_to_caller"}), {icon: "external-link", key: "editor.target.caller"});
    // 未知/空目标回退剪贴板结果（绑定前占位）
    assert.equal(targetDisplay(null).key, "editor.target.clipboard");
});

test("pickSendText prefers selection, falls back to full text", () => {
    assert.equal(pickSendText("  选区  ", "全文"), "  选区  ");
    assert.equal(pickSendText("   ", "全文"), "全文");
    assert.equal(pickSendText("", "全文"), "全文");
    assert.equal(pickSendText(null, "全文"), "全文");
});

test("menu items cover the five 0.23.2 actions + 0.23.6 recovery entry", () => {
    assert.deepEqual(MENU_ITEMS.map((i) => i.id), [
        "save-to", "save-copy", "copy-all", "create-sticky", "send-chat", "recover-draft",
    ]);
});

test("menu keyboard navigation wraps and supports Home/End", () => {
    assert.equal(nextMenuIndex(0, 5, "ArrowDown"), 1);
    assert.equal(nextMenuIndex(4, 5, "ArrowDown"), 0);
    assert.equal(nextMenuIndex(0, 5, "ArrowUp"), 4);
    assert.equal(nextMenuIndex(3, 5, "Home"), 0);
    assert.equal(nextMenuIndex(1, 5, "End"), 4);
    assert.equal(nextMenuIndex(1, 0, "ArrowDown"), -1);
});

test("saveTo: dialog cancel has no side effects", async () => {
    const {actions, log} = makeHarness();
    await actions.saveTo();
    assert.equal(log.dialogs.length, 1);
    assert.equal(log.commits.length, 0, "取消后不应发起提交");
});

test("saveTo switches target via commit override and reports target saved", async () => {
    const {actions, log} = makeHarness();
    actions.api.saveDialog = async () => "D:\\out.md";
    await actions.saveTo();
    assert.deepEqual(log.commits, [{kind: "save_to_file", path: "D:\\out.md"}]);
    assert.equal(log.targetSaved, 1);
});

test("saveCopy writes copy without changing main target", async () => {
    const {actions, session, log} = makeHarness();
    session.target = {kind: "clipboard_result"};
    actions.api.saveDialog = async () => "D:\\copy.md";
    session._result = {ok: true};
    await actions.saveCopy();
    assert.deepEqual(log.commits, [{kind: "save_copy_to_file", path: "D:\\copy.md"}]);
    assert.equal(session.target.kind, "clipboard_result", "副本不改变主目标");
});

test("saveCopy structured error reaches onError without status noise", async () => {
    const {actions, session, log} = makeHarness();
    actions.api.saveDialog = async () => "D:\\copy.md";
    session._result = {ok: false, error: {code: "io", message: "boom"}};
    await actions.saveCopy();
    assert.deepEqual(log.errors, [{code: "io", message: "boom"}]);
    assert.equal(log.status.length, 0);
});

test("copyAll copies full text via IPC", async () => {
    const {actions, log} = makeHarness();
    await actions.copyAll();
    assert.deepEqual(log.copies, ["全文内容"]);
});

test("createSticky 经统一 create_sticky Capability 创建（含显示窗口）且只调用一次", async () => {
    const {actions, log} = makeHarness();
    await actions.createSticky();
    // 只调一次原子入口：创建与显示由后端同一次调用完成，不存在二次创建
    assert.deepEqual(log.stickies, [{content: "全文内容"}]);
    assert.equal(log.status.at(-1), realT("editor.stickyCreated"));
});

test("createSticky 创建/显示失败时上报状态且不抛出", async () => {
    const {actions, api, log} = makeHarness();
    api._stickyError = new Error("窗口创建失败");
    await actions.createSticky();
    assert.equal(log.stickies.length, 1);
    assert.match(log.status.at(-1), /操作失败/);
});

test("sendToChat passes selection or full text as prefill via open_chat capability", async () => {
    const {actions, adapter, log} = makeHarness();
    adapter.selection = "选中段落";
    await actions.sendToChat();
    assert.deepEqual(log.chats, [{id: "open_chat", arg: {prefill: "选中段落"}}]);

    adapter.selection = "";
    await actions.sendToChat();
    assert.equal(log.chats[1].arg.prefill, "全文内容");
});

test("renderTarget uses session target and updates icon href", () => {
    const {actions, session} = makeHarness();
    session.target = {kind: "confirmed_file", path: "D:\\notes\\a.md"};
    const label = document.createElement("span");
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    actions.renderTarget(label, use);
    assert.ok(label.textContent.length > 0, "目标标签应非空");
    assert.equal(use.getAttribute("href"), "#icon-file-text");

    session.target = {kind: "update_sticky", stickyId: "s1"};
    actions.renderTarget(label, use);
    assert.equal(use.getAttribute("href"), "#icon-sticky-note");
});

// i18n 键完整性：防止字典漂移（t() 找不到 key 时回退 key 本身）
test("i18n dictionary contains all action/target keys", () => {
    const keys = [
        ...MENU_ITEMS.map((i) => i.key),
        "editor.target.clipboard",
        "editor.target.sticky",
        "editor.target.file",
        "editor.target.caller",
        // 0.23.7：顶部来源徽标
        "editor.source.empty",
        "editor.source.clipboard",
        "editor.source.sticky",
        "editor.source.selection",
        "editor.source.capability",
        // 0.23.7：整理等待态
        "editor.transform.stillWorking",
        "editor.transform.elapsed",
        "editor.transform.cancel",
        "editor.transform.runningHint",
        "editor.copied",
        "editor.stickyCreated",
        "editor.sentToChat",
        "editor.savedCopy",
        "editor.savedToFile",
        "editor.actionFailed",
        "editor.dirty",
        "editor.more",
        "editor.fileConflict",
        "editor.conflict.copy",
        "editor.conflict.reload",
        "editor.conflict.overwrite",
        "editor.conflict.saveCopy",
        // 0.23.6：恢复候选入口
        "editor.draft.dismiss",
        "editor.draft.deferred",
        "editor.draft.kept",
        "editor.draft.copyFailed",
        "editor.draft.restoreFailed",
        "editor.draft.staleFence",
        "editor.draft.flushFailed",
        "editor.draft.orphanFailed",
        "editor.draft.exitFlushFailed",
        "editor.draft.list.title",
        "editor.draft.list.meta",
        "editor.draft.list.empty",
        "editor.draft.list.failed",
        "editor.draft.kind.sticky",
        "editor.draft.kind.temporary",
        "editor.draft.orphanBadge",
        "editor.draft.restoreTo",
        "editor.draft.copyBtn",
        "editor.draft.deleteBtn",
        "editor.draft.deleteWarning",
        "editor.draft.deleteFailed",
        "editor.draft.deleted",
        "editor.draft.replaceWarning",
    ];
    for (const key of keys) {
        assert.notEqual(realT(key), key, `缺少 i18n 词条: ${key}`);
    }
});

// ── 0.23.6：choiceDialog 字符串契约映射（三出口各自正确分派）──────────

test("stickyConflictAction: ok=copy, third=reload, cancel/其余=stay", () => {
    assert.equal(stickyConflictAction("ok"), "copy");
    assert.equal(stickyConflictAction("third"), "reload");
    assert.equal(stickyConflictAction("cancel"), "stay");
    // 历史缺陷回归：布尔 true（旧契约残留）绝不能命中任何动作
    assert.equal(stickyConflictAction(true), "stay");
    assert.equal(stickyConflictAction(undefined), "stay");
});

test("fileConflictAction: ok=overwrite, third=saveCopy, cancel/其余=stay", () => {
    assert.equal(fileConflictAction("ok"), "overwrite");
    assert.equal(fileConflictAction("third"), "saveCopy");
    assert.equal(fileConflictAction("cancel"), "stay");
    assert.equal(fileConflictAction(true), "stay", "布尔残留不得触发覆盖");
});

test("recover-draft 菜单动作回调 onRecoverDraft", async () => {
    let opened = 0;
    const {actions} = makeHarness();
    actions._callbacks.onRecoverDraft = () => {
        opened += 1;
    };
    await actions.handleAction("recover-draft");
    assert.equal(opened, 1, "恢复入口经回调进入候选列表");
});
