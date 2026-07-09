//! 结果项激活动作：根据 action.kind 决定行为（打开应用 / 复制结果 / 运行内置动作）。
//!
//! 抽成独立模块，因为「激活」是 click 和键盘 Enter/Alt+数字的共同终点，
//! 且未来动作类型会增多（插件动作、文件打开等）——集中在此便于扩展。
//! 行为由后端提供的 action.kind 驱动，与提示栏（hints.js）同源，语义一致。

import { launchApp, runBuiltinAction, confirmAiAction, hideWindow, recordClipboardHit } from "./api.js";

/**
 * 激活一个结果项。
 * @param {{lnkPath?: string, calcValue?: string, payload?: string, action?: {kind: string, runId?: string, runArg?: any, hitId?: string}, isError?: boolean}} data
 */
export async function activateItem(data) {
  if (!data) return;
  // 错误信息项不可执行
  if (data.isError) return;

  // 0.9.2 第二步:AI Dangerous 动作确认卡片——Enter 确认执行
  if (data.aiConfirm) {
    try {
      await confirmAiAction(data.aiConfirm.actionName, data.aiConfirm.arguments);
    } catch (e) {
      console.error("confirm_ai_action failed:", e);
    }
    return;
  }

  const kind = data.action?.kind;

  if (kind === "copy") {
    // 复制结果到剪贴板；成功才隐藏。失败/空文本不隐藏，避免用户误以为已复制。
    // 插件 Copy 走 action.payload；计算结果无 payload 时回退 calcValue。
    const text = data.payload ?? data.calcValue;
    if (text) {
      try {
        await navigator.clipboard.writeText(text);
      } catch (e) {
        console.error("clipboard write failed:", e);
        return; // 保留窗口，让用户察觉并重试
      }
      // 0.8.5 §6.4：ClipboardEngine 展开的历史条目带 hitId → 回写频率加权（fire-and-forget）。
      // 失败不阻塞隐藏——回写不成功用户也已经复制到剪贴板，下次仍能召回。
      if (data.action?.hitId) {
        recordClipboardHit(data.action.hitId).catch((e) =>
          console.warn("record_clipboard_hit failed:", e)
        );
      }
      hideWindow();
    }
    // 无文本(空串/缺省):不复制也不隐藏,让用户察觉该结果无可复制内容
    return;
  }

  if (kind === "run") {
    // 内置动作（0.8.0 §1.3）：id 分派，后端 run_builtin_action 内部自动隐藏窗口
    // （OpenSettings 会先显设置窗、再隐主窗；无需前端调 hideWindow）。
    const id = data.action.runId;
    if (!id) {
      console.error("run action missing id");
      return;
    }
    try {
      await runBuiltinAction(id, data.action.runArg ?? null);
    } catch (e) {
      console.error("run_builtin_action failed:", e);
    }
    return;
  }

  // 默认 / open：启动应用（launch_app 内部会隐藏窗口）
  if (data.lnkPath) {
    try {
      await launchApp(data.lnkPath);
    } catch (e) {
      console.error("launch_app failed:", e);
    }
  }
}
