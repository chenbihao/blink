//! 动作 → 提示文案：把后端 action（{kind, hint}）映射成提示栏文案。
//! 纯函数，集中管理文案，新增动作类型只改这里。

/** 各 action.kind 的默认动作名（Enter 后接）。 */
const KIND_LABEL = {
  open: "打开",
  copy: "复制结果",
};

/**
 * 由 action 生成提示栏左侧文案。
 * @param {{kind: string, hint?: string}} action
 * @returns {string} 如 "Enter 打开"
 */
export function actionHint(action) {
  if (!action) return "";
  // 插件自定义动作名优先
  const label = action.hint || KIND_LABEL[action.kind] || "执行";
  return `Enter ${label}`;
}
