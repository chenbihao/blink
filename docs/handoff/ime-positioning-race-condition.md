# Handoff: IME 输入法定位异常（偶发）

> **创建时间**：20260812
> **状态**：已排查完成，待修复
> **严重度**：低（极低触发概率）
> **确信度**：中高（时序推断，缺直接复现日志）

---

## 现象

有时窗口打开后，输入法候选框定位在屏幕右上角而非输入框光标处，像是 IME 找不到输入框位置。极少出现，主窗口和便签窗口均有报告。

## 根因

IME 候选框位置由 Windows 系统通过 **ITextHost / ITextInputProvider** 接口从焦点元素获取光标坐标。当系统拿不到光标坐标时，IME 退到默认位置（屏幕右上角或 (0,0)）。

这是一个 **时序竞态问题**：`focus()` 调用早于 WebView2 完成布局（layout），IME 拿到无效坐标。

### 主窗口时序链

```
invoke()  [src/infra/platform/window/windows.rs:420]
  ├─ win.set_size()
  ├─ win.set_position()
  ├─ win.show()                      ← 窗口可见，但 WebView2 可能尚未完成 layout
  ├─ win.set_focus()                 ← Win32 HWND 获焦
  └─ app.emit(SHOWN)                 ← 异步事件 → 前端 queryEl.focus()
      └─ frontend/js/main-window/lifecycle.js:32
          └─ queryEl.focus()         ← JS 执行 focus，IME 据此定位
```

`emit(SHOWN)` 是异步投递。正常情况下前端 JS 执行时 WebView2 已完成 layout，`queryEl.focus()` 能让 IME 正确定位。但以下场景可能产生竞态：

1. **冷启动首次唤起**：WebView2 首次渲染未完成时 SHOWN 事件已到达前端，DOM 已存在但 **layout 尚未完成**，IME 拿到 (0,0) 坐标。
2. **窗口长时间隐藏后唤起**：WebView2 可能被系统挂起（suspended），恢复渲染需要额外时间。
3. **跨 DPI 切屏场景**：`set_position` 跨 DPI 屏会触发 `WM_DPICHANGED`，winit 按 DPI rescale 尺寸，此过程中 layout 不稳定。

### 便签窗口时序链（更高风险）

便签使用 **N+1 预热 spare 机制**，时序更紧张：

```
show_sticky_window() — 借用 spare 路径  [windows.rs:1578]
  ├─ spare_win.eval("__stickyReload('id')")   ← 异步 JS 注入，不等待执行完成
  ├─ spare_win.show()
  └─ spare_win.set_focus()
      └─ 前端 __stickyReload → loadStickyData → focusEditor()
          ├─ tiptapEditor.commands.focus()    ← Tiptap 已就绪时
          └─ textareaEl.focus()              ← Tiptap 未就绪时（降级）
```

关键问题：`eval()` 是 **fire-and-forget**，不等待 JS 执行完成。`show()` + `set_focus()` 可能在 `__stickyReload` 执行完之前就完成了。如果：

- **Tiptap bundle 尚未加载完**：`tiptapEditor` 为 `null`，走降级 `textareaEl.focus()`。但 Tiptap 模式下 `textareaEl` 是 `hidden` 的，focus 一个不可见元素 → IME 无法获取坐标 → 退到默认位置。
- **spare WebView2 刚创建尚未完成首次渲染**：即使 `textareaEl.focus()` 执行了，layout 也可能未完成。

## 触发条件

| 条件 | 概率 | 影响窗口 |
|---|---|---|
| WebView2 冷启动 / 从 suspended 恢复 | 极低 | 主窗口 + 便签 |
| spare 窗口刚创建 Tiptap 未加载 | 低 | 便签 |
| 跨 DPI 屏唤起 layout 抖动 | 极低 | 主窗口 |

## 涉及文件

| 文件 | 行号 | 作用 |
|---|---|---|
| `src/infra/platform/window/windows.rs` | 420-557 | `invoke()` — 主窗口唤起 |
| `src/infra/platform/window/windows.rs` | 1479-1763 | `show_sticky_window()` — 便签窗口显示 |
| `frontend/js/main-window/lifecycle.js` | 32-68 | SHOWN 事件处理 + `queryEl.focus()` |
| `frontend/js/sticky/main.js` | 454-461 | `focusEditor()` — Tiptap/textarea 降级聚焦 |

## 修复方向

### 方案 A：rAF 二次聚焦（最小改动）

在 `queryEl.focus()` 后追加 `requestAnimationFrame` 二次聚焦，确保 layout 完成后 IME 重新查询光标位置。

```javascript
// lifecycle.js SHOWN handler
queryEl.focus();
requestAnimationFrame(() => queryEl.focus());  // ← layout 完成后二次聚焦
```

便签 `focusEditor()` 同理：
```javascript
function focusEditor() {
    if (tiptapEditor) {
        tiptapEditor.commands.focus();
        requestAnimationFrame(() => tiptapEditor.commands.focus());
    } else {
        textareaEl.focus();
        requestAnimationFrame(() => textareaEl.focus());
    }
}
```

### 方案 B：便签 `eval` 改为等待回执

`__stickyReload` 改为返回 Promise，后端 `eval` + 等待前端回执后再 `show()` + `set_focus()`。

### 方案 C：Win32 IME 位置修正

focus 后用 Win32 `SendMessage` 给输入框发一次 `EM_SETSEL` 触发 IME 重新查询光标位置。

## 注意事项

- 修复时需读 `docs/specs/spec-frontend.md`（前端铁则）和 `docs/specs/spec-backend.md`（后端铁则）。
- 不要在 `invoke()` 的热路径中加 `sleep`——会破坏 <50ms 唤起延迟目标。
- rAF 方案对 <50ms 目标影响最小（rAF ~16ms，且不阻塞 Win32 调用）。
