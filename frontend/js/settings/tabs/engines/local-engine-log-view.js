/**
 * 引擎卡片日志区组件（0.22.6 从 local-engine-card.js 拆出）。
 *
 * - 日志文本绝不通过 innerHTML 注入，只走 textContent。
 * - instance_id 是内部标识，不展示；日志行只含时间/级别/文本。
 * - 用户上翻查看历史时跟随滚动：仅当已停在底部附近才自动拉底。
 *
 * @module local-engine-log-view
 */

import {tt, copyTextWithFeedback} from "./local-engine-card-utils.js";
import {formatLocalLogTimestamp, formatLogLine} from "./local-engine-log-format.js";

/**
 * 渲染日志区组件（工具栏 + 日志列表）。
 * @param {Object} entry - EngineStateEntry
 * @param {Object} controller
 * @param {Object} i18n
 * @returns {HTMLElement}
 */
export function renderLogComponent(entry, controller, i18n) {
    const wrapper = document.createElement("div");
    wrapper.className = "le-log-wrapper";

    // 工具栏
    const toolbar = document.createElement("div");
    toolbar.className = "le-log-toolbar";

    const copyBtn = document.createElement("button");
    copyBtn.className = "btn btn-small le-log-copy";
    copyBtn.textContent = tt(i18n, "local_engine.log.copy", "复制");
    copyBtn.addEventListener("click", () => {
        const text = entry.logs.map(formatLogLine).join("\n");
        copyTextWithFeedback(copyBtn, text, i18n);
    });
    toolbar.appendChild(copyBtn);

    const clearBtn = document.createElement("button");
    clearBtn.className = "btn btn-small le-log-clear";
    clearBtn.textContent = tt(i18n, "local_engine.log.clear", "清空");
    clearBtn.addEventListener("click", () => {
        controller.clearLogBuffer(entry.catalog.engine_id);
    });
    toolbar.appendChild(clearBtn);

    wrapper.appendChild(toolbar);

    // 日志列表
    const list = document.createElement("div");
    list.className = "le-log-list";
    updateLogList(list, entry, i18n);
    wrapper.appendChild(list);

    return wrapper;
}

/**
 * 更新日志列表内容（脏检查：长度 + 尾行 source:seq）。
 * @param {HTMLElement} list
 * @param {Object} entry
 * @param {Object} i18n
 */
export function updateLogList(list, entry, i18n) {
    // 脏检查：日志集合未变时跳过（长度 + 尾行 source:seq 唯一标识；
    // 截断丢头部时尾行 seq 变化仍会触发重建）。
    const logs = entry.logs;
    const last = logs[logs.length - 1];
    const sig = `${logs.length}:${last ? `${last.source}:${last.seq}` : ""}`;
    if (list.dataset.renderSig === sig) return;
    list.dataset.renderSig = sig;

    // 用户上翻查看历史日志时不要强制拉底——仅当已停在底部附近才跟随滚动。
    const nearBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 40;

    list.textContent = "";

    if (logs.length === 0) {
        const empty = document.createElement("div");
        empty.className = "le-log-empty";
        empty.textContent = tt(i18n, "local_engine.log.empty", "暂无日志");
        list.appendChild(empty);
        return;
    }

    for (const log of logs) {
        const line = document.createElement("div");
        line.className = `le-log-line le-log-${log.level || "info"}`;

        const time = document.createElement("span");
        time.className = "le-log-time";
        time.textContent = formatLocalLogTimestamp(log.timestamp);
        time.title = log.timestamp || "";

        const level = document.createElement("span");
        level.className = "le-log-level";
        level.textContent = log.level || "info";

        const text = document.createElement("span");
        text.className = "le-log-text";
        text.textContent = log.text; // textContent，绝不 innerHTML

        line.appendChild(time);
        line.appendChild(level);
        line.appendChild(text);
        list.appendChild(line);
    }

    // 滚动到底部（仅当更新前已在底部附近）
    if (nearBottom) {
        list.scrollTop = list.scrollHeight;
    }
}
