/**
 * 斜杠命令面板（0.27.7，phase §5.7 / §8.3 / §九）。
 *
 * 静态注册表（name/描述/图标）在本模块维护；动态内容（/tools 工具快照、
 * /mcp 连接状态、/skill 列表）经后端 command 拉取（get_slash_command_snapshot +
 * list_skills），不因查看产生连接副作用（phase §5.7.4）。
 *
 * 触发契约（§8.3）：仅 composer 首个非空白字符为 `/` 且整段输入单行时触发；
 * Tab/Enter 采纳、Esc 关闭不清输入、Shift+Enter 恒换行（composer 侧）、
 * IME 组合中不处理快捷采纳（composer 侧 guard）、未知命令保留草稿并提示。
 *
 * 互斥（§九）：面板经 composer-popups 仲裁，打开前关闭模型/思考下拉等其余
 * 弹层；异步结果回写前核对 composer inputRevision，过期即丢弃。
 */

import * as ipc from "./ipc.js";
import {iconHTML} from "../shared/icon.js";
import {escapeAttr, escapeText} from "./utils.js";
import {openExclusive, registerPopup} from "./composer-popups.js";
import {t} from "../i18n/index.js";

// ── 静态注册表（§8.3 定案：前端维护；动态内容走后端） ─────────────────────

/**
 * @typedef {{name: string, icon: string, argSyntax: string}} SlashCommand
 * `argSyntax`：空串 = 不接受参数；"/skill <名> [消息]" = 保留后端参数语义。
 */
export const SLASH_COMMANDS = [
    {name: "new", icon: "plus", argSyntax: ""},
    {name: "model", icon: "zap", argSyntax: ""},
    {name: "tools", icon: "terminal", argSyntax: ""},
    {name: "skill", icon: "bookmark", argSyntax: "/skill <名> [消息]"},
    {name: "mcp", icon: "server", argSyntax: ""},
    // 0.27.8（phase §5.8）
    {name: "compact", icon: "refresh-cw", argSyntax: ""},
    {name: "prompt", icon: "quote", argSyntax: ""},
];

/** 面板视图状态。 */
const MODE = {
    COMMANDS: "commands",
    SKILL: "skill",
    TOOLS: "tools",
    MCP: "mcp",
    PROMPT: "prompt",
};

/** @type {HTMLElement} 面板容器（chat.html 静态节点） */
let paletteEl = null;

/** @type {{name: string, args: string}|null} 当前触发输入的解析结果 */
let activeQuery = null;

/** @type {string|null} 当前视图 */
let mode = null;

/** @type {Array<object>} 当前可采纳行（command/skill 模式；详情视图为空） */
let items = [];

/** @type {number} 高亮行索引 */
let highlightIndex = 0;

/** 命令执行回调（main.js 注入：new/model/compact/prompt 等） */
let onCommand = null;

/** composer 输入代次读取（main.js 注入 composer.js 的 getter，避免模块循环依赖） */
let getInputRevision = () => 0;

/** 光标处插入文本（main.js 注入 composer.js 的 insertAtCursor，/prompt 采纳用） */
let insertTextAtCursor = null;

/** get_composer_bar 之外的第二数据源快照缓存（tools/mcp/skill 可用性共用） */
let snapshotCache = null;
let snapshotExpiry = 0;
const SNAPSHOT_TTL_MS = 5000;

/** skill 列表缓存（沿用旧 skill-hint 的 30s 缓存策略） */
let skillsCache = null;
let skillsExpiry = 0;
const SKILLS_TTL_MS = 30000;

/** 0.27.8: 提示词库（scope=chat）缓存——库变更事件（PROMPT_LIBRARY_CHANGED）
 * 由 main.js 订阅后调 invalidatePromptCache 失效（phase §5.8.5：库变更及时刷新）。 */
let promptCache = null;
let promptExpiry = 0;
const PROMPT_TTL_MS = 5000;

/** 当前面板渲染的 chat 提示词条目（id → 内容解析用，采纳时查 content） */
let promptEntries = [];

/**
 * 采纳 skill 填充 `/skill <名> ` 后抑制一次面板重开——填充会触发 input 事件，
 * 不抑制则刚采纳完面板立刻以该 skill 过滤重开（旧 skill-hint 填充不触发
 * input，无此问题）。用户后续继续输入仍正常触发过滤。
 */
let suppressNextSkillOpen = false;

// ── 纯函数（供 command-palette.test.mjs） ─────────────────────────────────

/**
 * 解析 composer 输入是否构成斜杠命令触发。
 *
 * 仅当整段输入单行、首个非空白字符为 `/` 时触发；多行、非 `/` 开头返回 null。
 * `/skill` 带参仍触发（skill 子视图过滤），其余命令带参由过滤层提示"不接受参数"。
 * @param {string} text
 * @returns {{name: string, args: string}|null} name 不含 `/` 前缀，可为空串
 */
export function splitSlashQuery(text) {
    if (typeof text !== "string" || text.includes("\n")) return null;
    const match = text.match(/^[ \t]*\/(\S*)(?:[ \t]+(.*))?$/);
    if (!match) return null;
    return {name: match[1].toLowerCase(), args: (match[2] || "").trim()};
}

/** 命令列表模糊过滤：前缀命中优先、大小写不敏感的子串命中次之。 */
export function filterSlashCommands(commands, name) {
    const q = (name || "").toLowerCase();
    if (!q) return [...commands];
    const prefixed = [];
    const contained = [];
    for (const cmd of commands) {
        if (cmd.name.startsWith(q)) prefixed.push(cmd);
        else if (cmd.name.includes(q)) contained.push(cmd);
    }
    return [...prefixed, ...contained];
}

/** 只列实际可激活的 Skill（沿用旧 skill-hint 语义，disabled 不进提示）。 */
export function filterActiveSkills(skills, query = "") {
    const normalizedQuery = query.trim().toLowerCase();
    return (Array.isArray(skills) ? skills : []).filter((skill) =>
        !skill.disabled &&
        (!normalizedQuery || String(skill.name || "").toLowerCase().includes(normalizedQuery))
    );
}

// ── 初始化与生命周期 ───────────────────────────────────────────────────────

/**
 * 初始化命令面板。
 * @param {{onCommand: (name: string) => void, getInputRevision?: () => number,
 *          insertTextAtCursor?: (text: string) => void}} callbacks
 *        main.js 注入命令执行器、composer 输入代次 getter（异步回写防过期）
 *        与光标插入函数（/prompt 采纳；均为避免模块循环依赖）
 */
export function initCommandPalette(callbacks) {
    paletteEl = document.getElementById("chat-command-palette");
    if (!paletteEl) return;
    onCommand = callbacks?.onCommand || null;
    if (typeof callbacks?.getInputRevision === "function") {
        getInputRevision = callbacks.getInputRevision;
    }
    if (typeof callbacks?.insertTextAtCursor === "function") {
        insertTextAtCursor = callbacks.insertTextAtCursor;
    }
    registerPopup("slash", closePalette);

    // 点击面板不 blur composer（§九：面板打开不 blur）；mousedown 阻止焦点转移
    paletteEl.addEventListener("mousedown", (e) => {
        if (!e.target.closest("button, input")) e.preventDefault();
    });
    // 事件委托：行点击 = 采纳并焦点回 composer
    paletteEl.addEventListener("click", (e) => {
        const row = e.target.closest("[data-adopt]");
        if (row) {
            adoptRow(String(row.dataset.adopt), row.dataset.value || "");
        }
    });

    // 点击面板外部关闭（与模型/思考下拉同惯例；textarea 点击不视为外部——
    // 点击 composer 继续输入属于正常编辑，面板由输入内容决定开合）
    document.addEventListener("click", (e) => {
        if (!isPaletteOpen()) return;
        if (paletteEl.contains(e.target)) return;
        const input = document.getElementById("chat-input");
        if (input && input.contains(e.target)) return;
        closePalette();
    });
}

/** 面板是否打开。 */
export function isPaletteOpen() {
    return mode !== null && paletteEl !== null && !paletteEl.hidden;
}

/** 配置/MCP/allowlist 变化时清缓存（main.js 订阅 config-changed 调用）。 */
export function invalidateSlashSnapshotCache() {
    snapshotCache = null;
    snapshotExpiry = 0;
    skillsCache = null;
    skillsExpiry = 0;
}

/** 0.27.8: 提示词库变更时清缓存（main.js 订阅 PROMPT_LIBRARY_CHANGED 调用）。 */
export function invalidatePromptCache() {
    promptCache = null;
    promptExpiry = 0;
}

/**
 * 0.27.8: 打开提示词选择面板（composer 快捷条按钮与 /prompt 共用入口，
 * phase §5.8.5）。与命令面板共用同一容器，经互斥仲裁打开。
 */
export function openPromptPalette() {
    if (!paletteEl) return;
    openView(MODE.PROMPT);
}

/** 关闭面板（不清输入；Esc/输入变化/采纳后调用）。 */
export function closePalette() {
    mode = null;
    activeQuery = null;
    items = [];
    if (paletteEl) paletteEl.hidden = true;
}

/**
 * composer input 事件入口：根据当前输入决定开/关/过滤。
 * @param {string} text textarea 当前值
 */
export function onComposerInput(text) {
    const parsed = splitSlashQuery(text);
    if (!parsed) {
        closePalette();
        return;
    }
    activeQuery = parsed;

    // /skill（含带参）→ skill 子视图；其余带参 → 命令表 + 无参提示
    if (parsed.name === "skill") {
        if (suppressNextSkillOpen && parsed.args) {
            // 刚采纳填充的 `/skill <名> `：跳过一次重开，用户继续输入再正常触发
            suppressNextSkillOpen = false;
            return;
        }
        openView(MODE.SKILL);
        return;
    }
    if (parsed.args) {
        // 已知命令但携带参数：保留草稿，提示不接受参数（§8.3 无效参数）
        openView(MODE.COMMANDS);
        return;
    }
    openView(MODE.COMMANDS);
}

/**
 * composer keydown 路由（仅面板打开且非 IME 组合时由 composer 调用）。
 * @param {KeyboardEvent} e
 * @returns {boolean} true = 已处理，composer 不应继续默认行为
 */
export function handlePaletteKeydown(e) {
    if (!isPaletteOpen()) return false;
    switch (e.key) {
        case "ArrowUp":
            e.preventDefault();
            moveHighlight(-1);
            return true;
        case "ArrowDown":
            e.preventDefault();
            moveHighlight(1);
            return true;
        case "Tab":
        case "Enter":
            if (e.shiftKey) return false; // Shift+Enter 恒换行（composer 处理）
            e.preventDefault();
            adoptHighlighted();
            return true;
        case "Escape":
            e.preventDefault();
            e.stopPropagation();
            closePalette();
            return true;
        default:
            return false;
    }
}

// ── 视图渲染 ───────────────────────────────────────────────────────────────

function openView(nextMode) {
    const wasOpen = isPaletteOpen();
    if (!wasOpen) openExclusive("slash"); // 互斥：关掉模型/思考下拉等（§九）
    mode = nextMode;
    highlightIndex = 0;

    if (nextMode === MODE.COMMANDS) {
        renderCommandList();
    } else if (nextMode === MODE.SKILL) {
        renderSkillList();
    } else if (nextMode === MODE.TOOLS) {
        renderToolsDetail();
    } else if (nextMode === MODE.MCP) {
        renderMcpDetail();
    } else if (nextMode === MODE.PROMPT) {
        renderPromptList();
    }
}

function commandRowHtml(cmd, {adoptable = true, hint = ""} = {}) {
    const desc = t(`chat.cmd.${cmd.name}.desc`);
    const argSyntax = cmd.argSyntax ? `<span class="cp-row-arg">${escapeText(cmd.argSyntax)}</span>` : "";
    return `<div class="cp-row${adoptable ? "" : " cp-row-static"}" data-adopt="${escapeAttr(cmd.name)}">
        <span class="cp-row-icon">${iconHTML(cmd.icon)}</span>
        <span class="cp-row-title">/${escapeText(cmd.name)}</span>
        ${argSyntax}
        <span class="cp-row-desc">${escapeText(desc)}${hint ? ` · ${escapeText(hint)}` : ""}</span>
    </div>`;
}

function renderCommandList() {
    const name = activeQuery?.name || "";
    const args = activeQuery?.args || "";
    const matched = filterSlashCommands(SLASH_COMMANDS, name);

    let html = "";
    if (matched.length === 0) {
        html = `<div class="cp-empty">${t("chat.cmd.unknown", {cmd: `/${name}`})}</div>`;
        items = [];
    } else {
        items = matched.map((cmd) => ({kind: "command", name: cmd.name}));
        // 已知命令带参且不接受参数 → 保留草稿 + 明确提示（§8.3）
        const noArgs = args && matched.length === 1 && matched[0].argSyntax === "";
        html = matched
            .map((cmd) => commandRowHtml(cmd, {
                adoptable: !noArgs,
                hint: noArgs ? t("chat.cmd.no_args") : "",
            }))
            .join("");
        if (noArgs) items = []; // 不可采纳，Enter/Tab 无效，草稿保留
    }
    paletteEl.innerHTML = html;
    showAndHighlight();
}

async function renderSkillList() {
    const revision = getInputRevision();
    paletteEl.innerHTML = `<div class="cp-loading">${escapeText(t("chat.cmd.loading"))}</div>`;
    paletteEl.hidden = false;

    const [snapshot, skills] = await Promise.all([
        loadSnapshot(),
        loadSkills(),
    ]);
    // 异步回写前校验：输入已变化或面板已关/已切视图 → 丢弃（§5.1）
    if (revision !== getInputRevision() || mode !== MODE.SKILL) return;

    if (snapshot?.mode === "pure_chat") {
        items = [];
        paletteEl.innerHTML = `<div class="cp-empty">${escapeText(t("chat.cmd.skill.pure_chat"))}</div>`;
        return;
    }
    if (!snapshot?.skills_enabled) {
        items = [];
        paletteEl.innerHTML = `<div class="cp-empty">${escapeText(t("chat.cmd.skill.disabled"))}</div>`;
        return;
    }

    const query = activeQuery?.args || "";
    const filtered = filterActiveSkills(skills, query);
    if (filtered.length === 0) {
        items = [];
        paletteEl.innerHTML = `<div class="cp-empty">${escapeText(
            query ? t("chat.cmd.skill.no_match", {name: query}) : t("chat.cmd.skill.empty")
        )}</div>`;
        return;
    }

    items = filtered.map((skill) => ({kind: "skill", value: skill.name}));
    paletteEl.innerHTML = filtered
        .map((skill) => {
            const sourceLabel = {blink: "Blink", claude: "Claude", zcode: "ZCode"}[skill.source] || skill.source;
            return `<div class="cp-row" data-adopt="skill" data-value="${escapeAttr(skill.name)}">
                <span class="cp-row-icon">${iconHTML("bookmark")}</span>
                <span class="cp-row-title">${escapeText(skill.name)}</span>
                <span class="cp-row-arg">${escapeText(sourceLabel)}</span>
                <span class="cp-row-desc">${escapeText(skill.description || "")}</span>
            </div>`;
        })
        .join("");
    showAndHighlight();
}

async function renderToolsDetail() {
    const revision = getInputRevision();
    paletteEl.innerHTML = `<div class="cp-loading">${escapeText(t("chat.cmd.loading"))}</div>`;
    paletteEl.hidden = false;

    const snapshot = await loadSnapshot();
    if (revision !== getInputRevision() || mode !== MODE.TOOLS) return;

    items = []; // 详情视图不可采纳，Esc 关闭
    const title = `<div class="cp-section-title">${escapeText(t("chat.cmd.tools.title"))}
        <span class="cp-section-count">${snapshot?.tools.length ?? 0}</span></div>`;
    let body;
    if ((snapshot?.tools || []).length === 0) {
        body = `<div class="cp-empty">${escapeText(
            snapshot?.tools_empty_reason === "pure_chat"
                ? t("chat.cmd.tools.empty_pure")
                : t("chat.cmd.tools.empty")
        )}</div>`;
    } else {
        const sourceLabel = snapshot.tools_source === "live"
            ? t("chat.cmd.tools.source_live")
            : t("chat.cmd.tools.source_estimate");
        body = `<div class="cp-note">${escapeText(sourceLabel)}</div>` +
            snapshot.tools
                .map((tool) => `<div class="cp-row cp-row-static">
                    <span class="cp-row-title">${escapeText(tool.name)}</span>
                    <span class="cp-row-arg">${escapeText(tool.source)}</span>
                    <span class="cp-row-desc">${escapeText(tool.description || "")}</span>
                </div>`)
                .join("");
    }
    paletteEl.innerHTML = title + body;
}

async function renderMcpDetail() {
    const revision = getInputRevision();
    paletteEl.innerHTML = `<div class="cp-loading">${escapeText(t("chat.cmd.loading"))}</div>`;
    paletteEl.hidden = false;

    const snapshot = await loadSnapshot();
    if (revision !== getInputRevision() || mode !== MODE.MCP) return;

    items = [];
    const servers = snapshot?.mcp_servers || [];
    const title = `<div class="cp-section-title">${escapeText(t("chat.cmd.mcp.title"))}
        <span class="cp-section-count">${servers.length}</span></div>`;
    let body;
    if (servers.length === 0) {
        body = `<div class="cp-empty">${escapeText(t("chat.cmd.mcp.empty"))}</div>`;
    } else {
        body = servers
            .map((server) => {
                const statusKey = `chat.cmd.mcp.status.${server.status}`;
                const statusLabel = t(statusKey);
                const toolsNote = server.status === "online"
                    ? t("chat.cmd.mcp.tool_count", {n: server.tool_count})
                    : "";
                return `<div class="cp-row cp-row-static">
                    <span class="cp-mcp-dot" data-status="${escapeAttr(server.status)}"></span>
                    <span class="cp-row-title">${escapeText(server.name)}</span>
                    <span class="cp-row-arg">${escapeText(server.transport)}</span>
                    <span class="cp-row-desc">${escapeText(statusLabel)}${toolsNote ? ` · ${escapeText(toolsNote)}` : ""}</span>
                </div>`;
            })
            .join("");
    }
    paletteEl.innerHTML = title + body;
}

/**
 * 0.27.8: 提示词选择面板（scope=chat 全部未隐藏条目，滚动列表——
 * 2026-10-23 定案不做最近使用筛选）。采纳 = 在光标/选区处插入正文，
 * 保留其他草稿与附件、不自动发送（phase §5.8.5）。
 */
async function renderPromptList() {
    const revision = getInputRevision();
    paletteEl.innerHTML = `<div class="cp-loading">${escapeText(t("chat.cmd.loading"))}</div>`;
    paletteEl.hidden = false;

    const entries = await loadChatPrompts();
    if (revision !== getInputRevision() || mode !== MODE.PROMPT) return;

    // 隐藏条目不进可选列表（后端已返回全量，由前端过滤）
    const visible = (entries || []).filter((entry) => !entry.hidden);
    if (visible.length === 0) {
        items = [];
        paletteEl.innerHTML = `<div class="cp-section-title">${escapeText(t("chat.cmd.prompt.title"))}</div>
            <div class="cp-empty">${escapeText(t("chat.cmd.prompt.empty"))}</div>`;
        return;
    }

    items = visible.map((entry) => ({kind: "prompt_entry", value: entry.id}));
    promptEntries = visible;
    paletteEl.innerHTML = `<div class="cp-section-title">${escapeText(t("chat.cmd.prompt.title"))}
            <span class="cp-section-count">${visible.length}</span></div>` +
        visible
            .map((entry) => `<div class="cp-row" data-adopt="prompt_entry" data-value="${escapeAttr(entry.id)}">
                <span class="cp-row-icon">${iconHTML("quote")}</span>
                <span class="cp-row-title">${escapeText(entry.name)}</span>
                ${entry.builtin ? `<span class="cp-row-arg">${escapeText(t("chat.cmd.prompt.builtin"))}</span>` : ""}
                <span class="cp-row-desc">${escapeText(t("chat.cmd.prompt.insert_hint"))}</span>
            </div>`)
            .join("");
    showAndHighlight();
}

function showAndHighlight() {
    paletteEl.hidden = false;
    applyHighlight();
}

function applyHighlight() {
    const rows = paletteEl.querySelectorAll(".cp-row[data-adopt]");
    rows.forEach((row, index) => {
        row.classList.toggle("cp-row-highlight", index === highlightIndex);
    });
    const highlighted = rows[highlightIndex];
    if (highlighted) highlighted.scrollIntoView({block: "nearest"});
}

function moveHighlight(delta) {
    const count = items.length;
    if (count === 0) return;
    highlightIndex = (highlightIndex + delta + count) % count;
    applyHighlight();
}

// ── 采纳 ───────────────────────────────────────────────────────────────────

function adoptHighlighted() {
    if (items.length === 0) return; // 未知命令/空列表：保留草稿不动作
    const item = items[Math.min(highlightIndex, items.length - 1)];
    if (item?.kind === "skill") adoptRow("skill", item.value);
    // 注意 kind 用 "prompt_entry" 而非 "prompt"——与命令名 /prompt 区分：
    // adoptRow 的首参在「命令名」与「条目 kind」两个命名空间复用，撞名会让
    // /prompt 命令误入条目分支（find("") 落空 → 静默无操作）
    else if (item?.kind === "prompt_entry") adoptRow("prompt_entry", item.value);
    else if (item?.kind === "command") adoptRow(item.name, "");
}

/**
 * 采纳一行。命令类（new/model/compact/prompt）→ 交 main.js 执行器并清空输入；
 * tools/mcp → 打开只读详情；skill → 填充 `/skill <名> ` 保留参数发送语义
 * （后端 parse_skill_command 不变）；prompt 条目 → 光标处插入正文
 * （保留其他草稿与附件、不自动发送，phase §5.8.5）。
 */
function adoptRow(kind, value) {
    if (kind === "prompt_entry") {
        const entry = promptEntries.find((item) => item.id === value);
        const content = entry?.effectiveContent ?? entry?.effective_content;
        closePalette();
        if (content) insertTextAtCursor?.(content);
        return;
    }
    closePalette();
    if (kind === "skill") {
        suppressNextSkillOpen = true;
        fillComposer(`/skill ${value} `);
        return;
    }
    if (kind === "tools" || kind === "mcp") {
        // 详情命令：打开只读视图，输入保持命令原文
        openView(kind === "tools" ? MODE.TOOLS : MODE.MCP);
        return;
    }
    // 执行型命令：清空输入（草稿即命令本身，无正文语义）
    fillComposer("");
    onCommand?.(kind);
}

/** 由 palette 写入 composer（skill 填充/命令清空），焦点回 composer（§九）。
 * 程序化整串写入后显式滚到末尾（spec-frontend §5.8）。 */
function fillComposer(text) {
    const input = document.getElementById("chat-input");
    if (!input) return;
    input.value = text;
    input.setSelectionRange(text.length, text.length);
    input.dispatchEvent(new Event("input", {bubbles: true}));
    input.focus();
    // spec-frontend §5.8：程序化写入不假设浏览器跟随光标，显式滚到末尾
    requestAnimationFrame(() => {
        input.scrollLeft = 0;
        input.scrollTop = input.scrollHeight;
    });
}

// ── 数据加载（带缓存 + revision 防护） ────────────────────────────────────

async function loadSnapshot() {
    const now = Date.now();
    if (snapshotCache && now < snapshotExpiry) return snapshotCache;
    try {
        const snapshot = await ipc.getSlashCommandSnapshot();
        snapshotCache = snapshot;
        snapshotExpiry = now + SNAPSHOT_TTL_MS;
        return snapshot;
    } catch (error) {
        console.warn("[command-palette] 快照加载失败:", error);
        return null;
    }
}

async function loadSkills() {
    const now = Date.now();
    if (skillsCache && now < skillsExpiry) return skillsCache;
    try {
        const {invoke} = await import("../shared/tauri.js");
        skillsCache = await invoke("list_skills");
        skillsExpiry = now + SKILLS_TTL_MS;
        return skillsCache;
    } catch (error) {
        console.warn("[command-palette] list_skills 失败:", error);
        return [];
    }
}

/** 0.27.8: 加载提示词库 scope=chat 条目（prompt_library_list 投影）。 */
async function loadChatPrompts() {
    const now = Date.now();
    if (promptCache && now < promptExpiry) return promptCache;
    try {
        promptCache = await ipc.listPromptLibrary("chat");
        promptExpiry = now + PROMPT_TTL_MS;
        return promptCache;
    } catch (error) {
        console.warn("[command-palette] 提示词库加载失败:", error);
        return [];
    }
}
