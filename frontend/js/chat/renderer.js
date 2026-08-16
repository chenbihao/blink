/**
 * chat Markdown 渲染器 — thin wrapper（0.16.2）。
 *
 * 核心渲染逻辑已抽取到 shared/markdown.js，本文件仅保留：
 * - re-export renderMarkdown / highlightCodeBlocks（chat 模块兼容）
 * - initRenderer：调 initMarkdown
 * - bindLinkOpener：chat 特有的链接点击拦截（绑定 #chat-messages）
 */

import {highlightCodeBlocks, initMarkdown, renderMarkdown} from "../shared/markdown.js";
import {invoke} from "../shared/tauri.js";

// Re-export 供 chat/components.js 使用
export {renderMarkdown, highlightCodeBlocks};

/**
 * 初始化渲染器。委托给共享模块。
 */
export function initRenderer() {
    initMarkdown();
}

/**
 * 绑定全局链接点击委托——拦截 chat-messages 内的 <a> 点击，
 * 通过后端 `open_url` command 在外部浏览器打开，防止 WebView 内部导航导致应用崩溃。
 *
 * 在 main.js init() 中调用一次。
 */
export function bindLinkOpener() {
    const messagesEl = document.getElementById("chat-messages");
    if (!messagesEl) return;
    messagesEl.addEventListener("click", (e) => {
        const link = e.target.closest("a[href]");
        if (!link) return;
        const href = link.getAttribute("href") || "";
        // 只拦截 http/https/mailto（与 DOMPurify 白名单一致）
        if (!/^(?:https?|mailto):/i.test(href)) return;
        e.preventDefault();
        // 用后端 open_url command（与设置页 openExternalUrl 一致）
        if (invoke) {
            invoke("open_url", {url: href}).catch((err) => {
                console.error("[chat] open_url 失败:", err);
                // 降级：window.open（在 Tauri 中可能无效，但不会崩溃）
                window.open(href, "_blank");
            });
        } else {
            window.open(href, "_blank");
        }
    });
}
