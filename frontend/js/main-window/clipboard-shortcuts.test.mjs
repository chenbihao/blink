//! clipboard-shortcuts 纯逻辑测试（0.20.8）。
//!
//! 运行：node --test frontend/js/main-window/clipboard-shortcuts.test.mjs
//!
//! 0.20.8: 快捷键从 Ctrl+E/Ctrl+D 改为 Alt+E/Alt+D（纯 Alt，排除 AltGr）。

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import {
  isImeComposing,
  isPureAlt,
  resolveShortcutAction,
  findEditAction,
  findDeleteTarget,
} from "./clipboard-shortcuts.js";

// ── 辅助：构造 KeyboardEvent mock ─────────────────────────────────────────

/**
 * 构造一个模拟的 KeyboardEvent 对象。
 * node:test 环境下没有真正的 KeyboardEvent 构造器（或行为不一致），
 * 用纯对象替代。
 */
function mockKey({
  key = "e",
  ctrlKey = false,
  metaKey = false,
  altKey = false,
  shiftKey = false,
  isComposing = false,
  keyCode = 0,
} = {}) {
  return { key, ctrlKey, metaKey, altKey, shiftKey, isComposing, keyCode };
}

// ── isImeComposing ─────────────────────────────────────────────────────────

describe("isImeComposing — IME 组字检测", () => {
  test("isComposing=true 时返回 true", () => {
    assert.ok(isImeComposing(mockKey({ isComposing: true })));
  });

  test("keyCode=229 时返回 true", () => {
    assert.ok(isImeComposing(mockKey({ keyCode: 229 })));
  });

  test("普通按键返回 false", () => {
    assert.ok(!isImeComposing(mockKey({ key: "e" })));
  });
});

// ── isPureAlt ──────────────────────────────────────────────────────────────

describe("isPureAlt — 纯 Alt 修饰键检测", () => {
  test("仅 Alt 按下返回 true", () => {
    assert.ok(isPureAlt(mockKey({ altKey: true })));
  });

  test("Alt+Shift 按下返回 false", () => {
    assert.ok(!isPureAlt(mockKey({ altKey: true, shiftKey: true })));
  });

  test("Alt+Ctrl（AltGr）返回 false", () => {
    assert.ok(!isPureAlt(mockKey({ altKey: true, ctrlKey: true })));
  });

  test("Alt+Meta 返回 false", () => {
    assert.ok(!isPureAlt(mockKey({ altKey: true, metaKey: true })));
  });

  test("无修饰键返回 false", () => {
    assert.ok(!isPureAlt(mockKey({})));
  });

  test("仅 Ctrl 返回 false", () => {
    assert.ok(!isPureAlt(mockKey({ ctrlKey: true })));
  });
});

// ── resolveShortcutAction ──────────────────────────────────────────────────

describe("resolveShortcutAction — 快捷键类型解析", () => {
  // ── Alt+E → edit ─────────────────────────────────────────────────────────

  test("Alt+E → edit", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "e", altKey: true }), true), "edit");
  });

  test("Alt+E 大写也能匹配", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "E", altKey: true }), true), "edit");
  });

  test("Alt+Shift+E → none（Shift 干扰纯 Alt）", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "E", altKey: true, shiftKey: true }), true), "none");
  });

  test("Ctrl+Alt+E（AltGr）→ none（Ctrl 干扰纯 Alt）", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "e", altKey: true, ctrlKey: true }), true), "none");
  });

  test("IME 组字时 Alt+E → none", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "e", altKey: true, isComposing: true }), true), "none");
  });

  test("Ctrl+E → none（已改为 Alt+E）", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "e", ctrlKey: true }), true), "none");
  });

  test("Meta+E → none（已改为 Alt+E，不再兼容 Mac）", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "e", metaKey: true }), true), "none");
  });

  // ── Alt+D → delete ───────────────────────────────────────────────────────

  test("Alt+D → delete", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "d", altKey: true }), true), "delete");
  });

  test("Alt+D 大写也能匹配", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "D", altKey: true }), true), "delete");
  });

  test("Alt+D query 非空时仍为 delete（Alt+D 不受 query 影响）", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "d", altKey: true }), false), "delete");
  });

  test("Alt+Shift+D → none（Shift 干扰纯 Alt）", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "d", altKey: true, shiftKey: true }), true), "none");
  });

  test("Ctrl+Alt+D（AltGr）→ none", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "d", altKey: true, ctrlKey: true }), true), "none");
  });

  test("IME 组字时 Alt+D → none", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "d", altKey: true, isComposing: true }), true), "none");
  });

  test("Ctrl+D → none（已改为 Alt+D）", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "d", ctrlKey: true }), true), "none");
  });

  // ── 裸 Delete ──────────────────────────────────────────────────────────────

  test("裸 Delete query 为空 → delete", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "Delete" }), true), "delete");
  });

  test("裸 Delete query 非空 → none（保留输入框前删字符）", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "Delete" }), false), "none");
  });

  test("Ctrl+Delete → none", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "Delete", ctrlKey: true }), true), "none");
  });

  test("Alt+Delete → none", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "Delete", altKey: true }), true), "none");
  });

  test("Shift+Delete → none", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "Delete", shiftKey: true }), true), "none");
  });

  test("IME 组字时 Delete → none", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "Delete", isComposing: true }), true), "none");
  });

  // ── 其他键 → none ──────────────────────────────────────────────────────────

  test("普通字母 → none", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "a" }), true), "none");
  });

  test("Ctrl+A → none（由 clipboard-mode handleKeydown 另行处理）", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "a", ctrlKey: true }), true), "none");
  });

  test("Ctrl+C → none（由 clipboard-mode handleKeydown 另行处理）", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "c", ctrlKey: true }), true), "none");
  });

  test("方向键 → none", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "ArrowDown" }), true), "none");
  });

  test("Enter → none", () => {
    assert.equal(resolveShortcutAction(mockKey({ key: "Enter" }), true), "none");
  });
});

// ── findEditAction ─────────────────────────────────────────────────────────

describe("findEditAction — 从 active 项查找 edit_text_item action", () => {
  test("有 edit_text_item action 时返回该 action", () => {
    const item = {
      isImage: false,
      actions: [
        { kind: "copy", hitId: "clip_1" },
        { kind: "run", runId: "edit_text_item", runArg: { originRef: "clip_1" } },
      ],
    };
    const result = findEditAction(item);
    assert.equal(result?.kind, "run");
    assert.equal(result?.runId, "edit_text_item");
    assert.equal(result?.runArg?.originRef, "clip_1");
  });

  test("无 edit_text_item action 时返回 null", () => {
    const item = {
      isImage: false,
      actions: [{ kind: "copy", hitId: "clip_1" }],
    };
    assert.equal(findEditAction(item), null);
  });

  test("图片项返回 null", () => {
    const item = {
      isImage: true,
      actions: [{ kind: "run", runId: "edit_text_item" }],
    };
    assert.equal(findEditAction(item), null);
  });

  test("无 active 项返回 null", () => {
    assert.equal(findEditAction(null), null);
  });

  test("actions 为空数组返回 null", () => {
    assert.equal(findEditAction({ isImage: false, actions: [] }), null);
  });

  test("actions 为 undefined 返回 null", () => {
    assert.equal(findEditAction({ isImage: false }), null);
  });

  test("有多个 action 时仍能找到 edit_text_item", () => {
    const item = {
      isImage: false,
      actions: [
        { kind: "copy", hitId: "clip_1" },
        { kind: "run", runId: "pin_text_item" },
        { kind: "run", runId: "edit_text_item", runArg: { originRef: "clip_1" } },
      ],
    };
    const result = findEditAction(item);
    assert.equal(result?.runId, "edit_text_item");
  });
});

// ── findDeleteTarget ───────────────────────────────────────────────────────

describe("findDeleteTarget — 从 active 项解析删除目标", () => {
  test("文本项返回 { type: 'text', id: hitId }", () => {
    const item = {
      isImage: false,
      source: "clipboard",
      actions: [{ kind: "copy", hitId: "clip_42" }],
    };
    const result = findDeleteTarget(item);
    assert.deepEqual(result, { type: "text", id: "clip_42" });
  });

  test("图片项返回 { type: 'image', id: lnkPath }", () => {
    const item = {
      isImage: true,
      source: "clipboard",
      lnkPath: "img_99",
      actions: [{ kind: "run", runId: "copy_clipboard_image", runArg: "img_99" }],
    };
    const result = findDeleteTarget(item);
    assert.deepEqual(result, { type: "image", id: "img_99" });
  });

  test("非 clipboard 来源返回 null（颜色降级结果不可删除）", () => {
    const item = {
      isImage: false,
      source: "color",
      actions: [{ kind: "copy", payload: "#ff0000" }],
    };
    assert.equal(findDeleteTarget(item), null);
  });

  test("source 为 undefined 返回 null", () => {
    const item = {
      isImage: false,
      actions: [{ kind: "copy", hitId: "clip_1" }],
    };
    assert.equal(findDeleteTarget(item), null);
  });

  test("无 active 项返回 null", () => {
    assert.equal(findDeleteTarget(null), null);
  });

  test("文本项缺少 hitId 返回 null", () => {
    const item = {
      isImage: false,
      source: "clipboard",
      actions: [{ kind: "copy" }],
    };
    assert.equal(findDeleteTarget(item), null);
  });

  test("图片项缺少 lnkPath 返回 null", () => {
    const item = {
      isImage: true,
      source: "clipboard",
      actions: [{ kind: "run", runId: "copy_clipboard_image" }],
    };
    assert.equal(findDeleteTarget(item), null);
  });

  test("actions 为空数组返回 null", () => {
    const item = {
      isImage: false,
      source: "clipboard",
      actions: [],
    };
    assert.equal(findDeleteTarget(item), null);
  });
});
