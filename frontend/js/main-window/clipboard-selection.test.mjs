//! clipboard-selection 纯逻辑测试（0.20.2）。
//!
//! 运行：node --test frontend/js/main-window/clipboard-selection.test.mjs

import { test, describe, beforeEach } from "node:test";
import assert from "node:assert/strict";

import {
  getSelectedKeys,
  hasSelection,
  selectedCount,
  getSelectionEpoch,
  getCopyGeneration,
  isSelected,
  onEnterMode,
  onExitMode,
  onWindowHidden,
  onQueryChanged,
  clearSelection,
  toggleSelection,
  selectAll,
  reconcileAfterReorder,
  beginCopy,
  isCopyStillValid,
  _resetForTest,
} from "./clipboard-selection.js";

beforeEach(() => {
  _resetForTest();
});

describe("toggleSelection — 单击切换", () => {
  test("首次选中加入集合", () => {
    const result = toggleSelection("clip_1");
    assert.equal(result, true);
    assert.ok(isSelected("clip_1"));
    assert.equal(selectedCount(), 1);
  });

  test("再次切换取消选中", () => {
    toggleSelection("clip_1");
    const result = toggleSelection("clip_1");
    assert.equal(result, false);
    assert.ok(!isSelected("clip_1"));
    assert.equal(selectedCount(), 0);
  });

  test("多个 key 独立切换", () => {
    toggleSelection("clip_1");
    toggleSelection("clip_2");
    toggleSelection("clip_3");
    assert.equal(selectedCount(), 3);
    assert.ok(isSelected("clip_1"));
    assert.ok(isSelected("clip_2"));
    assert.ok(isSelected("clip_3"));
  });

  test("空 key 不选中", () => {
    const result = toggleSelection("");
    assert.equal(result, false);
    assert.equal(selectedCount(), 0);
  });
});

describe("selectAll — Ctrl+A 全选文本项", () => {
  test("全选当前所有文本项", () => {
    const keys = ["clip_1", "clip_2", "clip_3"];
    selectAll(keys);
    assert.equal(selectedCount(), 3);
    assert.ok(isSelected("clip_1"));
    assert.ok(isSelected("clip_2"));
    assert.ok(isSelected("clip_3"));
  });

  test("空列表全选无效果", () => {
    selectAll([]);
    assert.equal(selectedCount(), 0);
  });
});

describe("reconcileAfterReorder — 翻页/重排后保留", () => {
  test("保留仍存在的 key", () => {
    toggleSelection("clip_1");
    toggleSelection("clip_2");
    toggleSelection("clip_3");
    // 重排后 clip_2 消失
    reconcileAfterReorder(["clip_1", "clip_3", "clip_4"]);
    assert.ok(isSelected("clip_1"));
    assert.ok(isSelected("clip_3"));
    assert.ok(!isSelected("clip_2"));
    assert.equal(selectedCount(), 2);
  });

  test("全部消失后清空", () => {
    toggleSelection("clip_1");
    toggleSelection("clip_2");
    reconcileAfterReorder(["clip_3", "clip_4"]);
    assert.equal(selectedCount(), 0);
  });

  test("增量到达后新增项不影响已有选择", () => {
    toggleSelection("clip_1");
    reconcileAfterReorder(["clip_1", "clip_2", "clip_3"]);
    assert.ok(isSelected("clip_1"));
    assert.equal(selectedCount(), 1); // 不自动扩展
  });
});

describe("epoch/generation — 竞态防护", () => {
  test("onEnterMode 递增 epoch 并清空", () => {
    toggleSelection("clip_1");
    const epoch1 = getSelectionEpoch();
    onEnterMode();
    const epoch2 = getSelectionEpoch();
    assert.equal(epoch2, epoch1 + 1);
    assert.equal(selectedCount(), 0);
  });

  test("onExitMode 递增 epoch + generation 并清空", () => {
    toggleSelection("clip_1");
    const epoch1 = getSelectionEpoch();
    const gen1 = getCopyGeneration();
    onExitMode();
    assert.equal(getSelectionEpoch(), epoch1 + 1);
    assert.equal(getCopyGeneration(), gen1 + 1);
    assert.equal(selectedCount(), 0);
  });

  test("onWindowHidden 递增 generation 并清空", () => {
    toggleSelection("clip_1");
    const gen = beginCopy();
    onWindowHidden();
    assert.ok(!isCopyStillValid(gen));
    assert.equal(selectedCount(), 0);
  });

  test("onQueryChanged 递增 generation 并清空", () => {
    toggleSelection("clip_1");
    const gen = beginCopy();
    onQueryChanged();
    assert.ok(!isCopyStillValid(gen));
    assert.equal(selectedCount(), 0);
  });

  test("beginCopy 返回新 generation，isCopyStillValid 比对", () => {
    toggleSelection("clip_1");
    const gen = beginCopy();
    assert.ok(isCopyStillValid(gen));
    // 未发生 epoch/generation 变化
    assert.ok(isCopyStillValid(gen));
  });

  test("beginCopy 后 onExitMode 使 generation 失效", () => {
    toggleSelection("clip_1");
    const gen = beginCopy();
    onExitMode();
    assert.ok(!isCopyStillValid(gen));
  });

  test("beginCopy 后 onQueryChanged 使 generation 失效", () => {
    toggleSelection("clip_1");
    const gen = beginCopy();
    onQueryChanged();
    assert.ok(!isCopyStillValid(gen));
  });
});

describe("clearSelection — 基础清空", () => {
  test("清空选中但不清 epoch/generation", () => {
    onEnterMode();
    toggleSelection("clip_1");
    const epoch = getSelectionEpoch();
    const gen = getCopyGeneration();
    clearSelection();
    assert.equal(selectedCount(), 0);
    assert.equal(getSelectionEpoch(), epoch); // epoch 不变
    assert.equal(getCopyGeneration(), gen); // gen 不变
  });
});
