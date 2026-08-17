//! 动作 → 提示文案：把后端 action（{kind, hint}）映射成提示栏文案。
//! 纯函数，集中管理文案，新增动作类型只改这里。
//!
//! 返回 `{ template, params }` 二元组供 `renderHint` 走键帽渲染——
//! template 里的 `{{key:Enter}}` 会被替换成 <kbd> Element，`{label}` 走参数注入。

import {t} from "../i18n/index.js";

/** 各 action.kind 默认动作名的 i18n key（Enter 后接）。 */
const KIND_KEY = {
    open: "hint.open",
    copy: "hint.copy",
};

/**
 * 由 action 生成提示栏左侧模板 + 参数。
 * @param {{kind: string, hint?: string}} action
 * @param {boolean} [clipboardMode] 0.21.x：剪贴板模式下文本项 Enter = 上屏（复制兜底），
 *   提示文案与普通「复制」区分。
 * @returns {{template: string, params: Record<string, string>}}
 *   如 `{ template: "{{key:Enter}} {label}", params: { label: "打开" } }`
 */
export function actionHint(action, clipboardMode) {
    if (!action) return {template: "", params: {}};
    // 插件自定义动作名（action.hint，来自 manifest）原样使用不翻译；默认动作名走 i18n
    const key = clipboardMode && action.kind === "copy" ? "hint.paste_to_input" : KIND_KEY[action.kind];
    const label = action.hint || (key ? t(key) : t("hint.fallback"));
    return {template: t("hint.enter"), params: {label}};
}
