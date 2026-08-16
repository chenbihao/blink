//! 结果项激活动作：根据 action.kind 决定行为（打开应用 / 复制结果 / 运行内置动作）。
//!
//! 抽成独立模块，因为「激活」是 click 和键盘 Enter/Alt+数字的共同终点，
//! 且未来动作类型会增多（插件动作、文件打开等）——集中在此便于扩展。
//! 行为由后端提供的 action.kind 驱动，与提示栏（hints.js）同源，语义一致。
//!
//! 0.20.0：所有 catch 块统一走 action-error.js 的 showActionError，
//! 不再静默吞错——内置动作失败进入可见 statusbar 反馈。

import {
    copyClipboardImage,
    createStickyNote,
    getClipboardText,
    hideWindow,
    launchApp,
    openContentEditor,
    pinClipboardImage,
    recordClipboardHit,
    runBuiltinAction,
    showStickyWindow
} from "../shared/api.js";
import {showActionError} from "./action-error.js";

/**
 * 激活一个结果项。
 * 0.16.1: 从 data.actions[0] 取首个动作执行（回车/左键点击场景）。
 * 右键菜单点击特定动作时，调用方传 { ...readData(li), actions: [specificAction] }。
 * 0.17.6: AI 确认卡片已移至 ai-mode.js，此函数不再处理 aiConfirm。
 * @param {{lnkPath?: string, calcValue?: string, actions?: Array<{kind: string, hint?: string, payload?: string, runId?: string, runArg?: any, hitId?: string}>, isError?: boolean}} data
 */
export async function activateItem(data) {
    if (!data) return;
    // 错误信息项不可执行
    if (data.isError) return;

    const action = data.actions?.[0];
    const kind = action?.kind;

    if (kind === "copy") {
        // 复制结果到剪贴板；成功才隐藏。失败/空文本不隐藏，避免用户误以为已复制。
        // 插件 Copy / 计算结果走 action.payload；剪贴板历史走 LazyCopy（payload 为空，
        // hitId 非空时按需调 getClipboardText 拉取完整 text）。
        let text = action.payload ?? data.calcValue;
        if (!text && action.hitId) {
            // LazyCopy：搜索路径未预载 text，激活时按需拉取
            try {
                text = await getClipboardText(action.hitId);
            } catch (e) {
                showActionError("get_clipboard_text", e);
                return; // 保留窗口，让用户察觉并重试
            }
        }
        if (text) {
            try {
                await navigator.clipboard.writeText(text);
            } catch (e) {
                showActionError("clipboard_write", e);
                return; // 保留窗口，让用户察觉并重试
            }
            // 0.8.5 §6.4：ClipboardEngine 展开的历史条目带 hitId → 回写频率加权（fire-and-forget）。
            // 失败不阻塞隐藏——回写不成功用户也已经复制到剪贴板，下次仍能召回。
            if (action.hitId) {
                recordClipboardHit(action.hitId).catch((e) =>
                    console.warn("record_clipboard_hit failed:", e)
                );
            }
            hideWindow();
        }
        // 无文本(空串/缺省):不复制也不隐藏,让用户察觉该结果无可复制内容
        return;
    }

    if (kind === "run") {
        const id = action.runId;
        if (!id) {
            console.error("run action missing id");
            return;
        }

        // 0.16.4：剪贴板图片复制——不走 run_builtin_action，直调专用命令
        if (id === "copy_clipboard_image") {
            const imageId = typeof action.runArg === "string" ? action.runArg : null;
            if (!imageId) {
                console.error("copy_clipboard_image: missing imageId");
                return;
            }
            try {
                await copyClipboardImage(imageId);
            } catch (e) {
                showActionError("copy_clipboard_image", e);
                return; // 失败不隐藏，让用户察觉
            }
            hideWindow();
            return;
        }

        // 0.16.5：剪贴板图片钉图——不走 run_builtin_action，直调专用命令
        if (id === "pin_clipboard_image") {
            const imageId = typeof action.runArg === "string" ? action.runArg : null;
            if (!imageId) {
                console.error("pin_clipboard_image: missing imageId");
                return;
            }
            try {
                await pinClipboardImage(imageId);
            } catch (e) {
                showActionError("pin_clipboard_image", e);
                return;
            }
            hideWindow();
            return;
        }

        // P2-#18: 文本型 item 的编辑动作——打开内容编辑器
        if (id === "edit_text_item") {
            const arg = action.runArg;
            let text = arg?.text ?? "";
            const originRef = arg?.originRef ?? null;
            const source = arg?.source ?? "item";
            // LazyCopy：runArg 不含 text 字段时，按 originRef 拉取完整 text
            if (!text && originRef) {
                try {
                    text = (await getClipboardText(originRef)) ?? "";
                } catch (e) {
                    showActionError("get_clipboard_text", e);
                }
            }
            const isClipboard = source === "clipboard";
            try {
                await openContentEditor({
                    body: text,
                    format: "plain",
                    title: isClipboard ? "编辑剪贴板内容" : "编辑内容",
                    origin: isClipboard ? "clipboard" : "item",
                    originRef,
                    savePolicy: "clipboard_new",
                });
            } catch (e) {
                showActionError("edit_text_item", e);
            }
            hideWindow();
            return;
        }

        // P2-#18: 文本型 item 的钉为便签动作
        // LazyCopy：runArg 现在是 { originRef } 对象（旧版为 string），兼容两种
        if (id === "pin_text_item") {
            const arg = action.runArg;
            let text = "";
            if (typeof arg === "string") {
                text = arg;
            } else if (arg?.originRef) {
                try {
                    text = (await getClipboardText(arg.originRef)) ?? "";
                } catch (e) {
                    showActionError("get_clipboard_text", e);
                }
            }
            try {
                const note = await createStickyNote(text);
                await showStickyWindow(note.id, true);
            } catch (e) {
                showActionError("pin_text_item", e);
            }
            hideWindow();
            return;
        }

        // 内置动作（0.8.0 §1.3）：id 分派，后端 run_builtin_action 内部自动隐藏窗口
        // （OpenSettings 会先显设置窗、再隐主窗；无需前端调 hideWindow）。
        try {
            await runBuiltinAction(id, action.runArg ?? null);
        } catch (e) {
            showActionError("run_builtin_action", e);
        }
        return;
    }

    // 默认 / open：启动应用（launch_app 内部会隐藏窗口）
    if (data.lnkPath) {
        try {
            await launchApp(data.lnkPath);
        } catch (e) {
            showActionError("launch_app", e);
        }
    }
}
