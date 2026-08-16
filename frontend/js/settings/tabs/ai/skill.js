//! AI Tab Skill 列表 + 导入面板 + CLI 识别（0.14.6 §4.2 拆分）。
//!
//! 包含：
//! - loadSkillList — 加载并渲染已发现的 Skill 列表
//! - showSkillImportPanel / initSkillImportHandlers / closeSkillImportPanel — 导入面板
//! - renderSkillImportList / updateSkillImportSourceInfo / updateImportConfirmState — 导入列表
//! - doSkillImportSelected / scanCustomDirForSkills — 导入执行
//! - showSkillEditModal — Skill 编辑 modal
//! - CLI 识别 IIFE — 浏览文件 + 识别按钮 + Enter 触发

import {aiState, escapeAttr, escapeHtml} from "./state.js";
import {confirmDialog, invoke} from "../../../shared/tauri.js";
import {t} from "../../../i18n/index.js";
import {iconHTML} from "../../../shared/icon.js";

/**
 * 加载并渲染已发现的 Skill 列表（0.13.6 增强版）。
 */
export async function loadSkillList() {
    const container = document.getElementById("ai-skill-list");
    if (!container) return;

    try {
        const skills = await invoke("list_skills");
        if (!skills || skills.length === 0) {
            container.innerHTML = `<span class="skill-empty-hint">${t("ai.skill.list.empty") || "点击刷新扫描 Skill 目录"}</span>`;
            return;
        }

        container.innerHTML = skills.map((s) => {
            const sourceLabel = {
                blink: "Blink",
                claude: "Claude",
                zcode: "ZCode",
                opencode: "OpenCode",
                codex: "Codex"
            }[s.source] || s.source;
            const triggerBadge = s.triggers
                ? `<span class="skill-badge skill-badge-trigger">${t("ai.skill.badge.auto") || "自动触发"}</span>`
                : `<span class="skill-badge skill-badge-manual">${t("ai.skill.badge.manual") || "手动"}</span>`;
            const skillId = `${s.name}@${s.source}`;
            const isChecked = !s.disabled ? "checked" : "";
            const cliBadge = s.source_cli_path
                ? `<span class="skill-badge skill-badge-cli" title="来源 CLI: ${escapeAttr(s.source_cli_path)}">CLI</span>`
                : "";
            const kwInline = s.triggers?.keywords?.length
                ? `<span class="skill-keywords-inline" title="触发关键词: ${escapeAttr(s.triggers.keywords.join(", "))}">${s.triggers.keywords.map((k) => escapeHtml(k)).map((k) => `#${k}`).join(" ")}</span>`
                : "";
            return `
        <div class="skill-item${s.disabled ? " skill-item-disabled" : ""}">
          <div class="skill-item-header">
            <label class="checkbox skill-toggle" title="启用/禁用此 Skill">
              <input type="checkbox" class="skill-toggle-input" data-skill-id="${escapeAttr(skillId)}" ${isChecked} />
              <span class="checkmark"></span>
            </label>
            <span class="skill-item-name">${escapeHtml(s.name)}</span>
            <span class="skill-badge skill-badge-source">${sourceLabel}</span>
            ${cliBadge}
            ${triggerBadge}
            ${kwInline}
            <div class="skill-item-actions">
              <button class="btn btn-icon-sm skill-edit-btn" data-dir="${escapeAttr(s.dir)}" title="编辑">${iconHTML("pencil")}</button>
              ${s.source_cli_path ? `<button class="btn btn-icon-sm skill-regenerate-btn" data-cli-path="${escapeAttr(s.source_cli_path)}" data-skill-dir="${escapeAttr(s.dir)}" title="重新识别">${iconHTML("refresh-cw")}</button>` : ""}
              <button class="btn btn-icon-sm skill-open-skill-dir" data-dir="${escapeAttr(s.dir)}" title="打开目录">${iconHTML("folder-open")}</button>
              <button class="btn btn-icon-sm skill-delete-btn" data-dir="${escapeAttr(s.dir)}" title="删除">${iconHTML("x")}</button>
            </div>
          </div>
          <div class="skill-item-desc">${escapeHtml(s.description)}</div>
        </div>`;
        }).join("");

        // 事件绑定——复选框切换
        container.querySelectorAll(".skill-toggle-input").forEach((cb) => {
            cb.addEventListener("change", async (e) => {
                const skillId = e.target.dataset.skillId;
                const enabled = e.target.checked;
                try {
                    await invoke("set_skill_enabled", {skillId, enabled});
                    const item = e.target.closest(".skill-item");
                    if (item) item.classList.toggle("skill-item-disabled", !enabled);
                } catch (err) {
                    console.error("set_skill_enabled failed:", err);
                    e.target.checked = !enabled;
                    const item = e.target.closest(".skill-item");
                    if (item) item.classList.toggle("skill-item-disabled", enabled);
                }
            });
        });

        // 编辑 Skill
        container.querySelectorAll(".skill-edit-btn").forEach((btn) => {
            btn.addEventListener("click", async () => {
                const skillDir = btn.dataset.dir;
                if (!skillDir) return;
                try {
                    const content = await invoke("get_skill_content", {skillDir});
                    showSkillEditModal(skillDir, content, null);
                } catch (e) {
                    console.error("get_skill_content failed:", e);
                }
            });
        });

        // 重新生成 CLI Skill
        container.querySelectorAll(".skill-regenerate-btn").forEach((btn) => {
            btn.addEventListener("click", async () => {
                const cliPath = btn.dataset.cliPath;
                if (!cliPath) return;
                try {
                    const result = await invoke("recognize_cli_tool", {cliPath});
                    showSkillEditModal(result.saved_path.replace(/SKILL\.md$/, ""), result.skill_md_content, cliPath);
                } catch (e) {
                    console.error("regenerate skill failed:", e);
                }
            });
        });

        // 删除 Skill
        container.querySelectorAll(".skill-delete-btn").forEach((btn) => {
            btn.addEventListener("click", async () => {
                const skillDir = btn.dataset.dir;
                if (!skillDir) return;
                const ok = await confirmDialog(`确认删除此 Skill？`, {title: "确认", kind: "warning"});
                if (!ok) return;
                try {
                    await invoke("delete_skill", {skillDir});
                    await invoke("refresh_skills");
                    await loadSkillList();
                } catch (e) {
                    console.error("delete_skill failed:", e);
                }
            });
        });

        // 打开 Skill 目录
        container.querySelectorAll(".skill-open-skill-dir").forEach((btn) => {
            btn.addEventListener("click", async () => {
                const dir = btn.dataset.dir;
                if (!dir) return;
                try {
                    await invoke("open_dir_in_explorer", {path: dir});
                } catch (e) {
                    console.error("open skill dir failed:", e);
                }
            });
        });
    } catch (e) {
        container.innerHTML = `<span class="skill-empty-hint">${escapeHtml(e)}</span>`;
        console.error("loadSkillList failed:", e);
    }
}

// ── Skill 导入面板 ──────────────────────────────────────────

/** 打开导入面板并加载来源数据。 */
export async function showSkillImportPanel() {
    const overlay = document.getElementById("skill-import-overlay");
    if (!overlay) return;
    const errorEl = document.getElementById("skill-import-error");
    if (errorEl) errorEl.textContent = "";
    const listEl = document.getElementById("skill-import-skill-list");
    if (listEl) listEl.innerHTML = '<span class="skill-empty-hint">加载中...</span>';
    aiState._customImportDir = null;
    overlay.classList.remove('hidden');

    try {
        aiState._skillImportSourcesCache = await invoke("list_external_skill_sources");
    } catch (e) {
        aiState._skillImportSourcesCache = [];
        if (listEl) listEl.innerHTML = `<span class="skill-empty-hint">加载失败: ${escapeHtml(String(e))}</span>`;
        console.error("list_external_skill_sources failed:", e);
    }
    populateSkillImportSourceSelect();
    renderSkillImportList(null);
}

/** 填充来源下拉选项。 */
function populateSkillImportSourceSelect() {
    const select = document.getElementById("skill-import-source-app");
    if (!select) return;
    const sources = aiState._skillImportSourcesCache || [];
    select.innerHTML = '<option value="">— 选择来源 —</option>';
    for (const s of sources) {
        const opt = document.createElement("option");
        opt.value = s.id;
        const existsTag = s.exists ? `（${s.skills.length} 个）` : "（目录不存在）";
        opt.textContent = `${s.label} ${existsTag}`;
        opt.dataset.sourceId = s.id;
        if (!s.exists) opt.disabled = true;
        select.appendChild(opt);
    }
    const customOpt = document.createElement("option");
    customOpt.value = "__custom__";
    customOpt.textContent = "自定义文件夹...";
    select.appendChild(customOpt);
    select.value = "";
}

/** 绑定导入面板内的事件（只绑定一次）。 */
export function initSkillImportHandlers() {
    const select = document.getElementById("skill-import-source-app");
    select?.addEventListener("change", async () => {
        const val = select.value;
        if (val === "__custom__") {
            try {
                const dir = await invoke("pick_directory_dialog", {title: "选择 Skill 所在目录"});
                if (!dir) {
                    select.value = "";
                    aiState._customImportDir = null;
                    renderSkillImportList(null);
                    updateSkillImportSourceInfo(null);
                    return;
                }
                aiState._customImportDir = dir;
                const customSource = {
                    id: "__custom__",
                    label: "自定义",
                    dir,
                    exists: true,
                    skills: await scanCustomDirForSkills(dir),
                };
                renderSkillImportList(customSource);
                updateSkillImportSourceInfo(customSource);
            } catch (e) {
                console.error("pick custom skill dir failed:", e);
                select.value = "";
                aiState._customImportDir = null;
            }
            return;
        }
        aiState._customImportDir = null;
        const src = (aiState._skillImportSourcesCache || []).find((s) => s.id === val) || null;
        renderSkillImportList(src);
        updateSkillImportSourceInfo(src);
    });

    document.getElementById("skill-import-open-source-dir")?.addEventListener("click", () => {
        const src = currentImportSourceDir();
        if (!src) return;
        invoke("open_dir_in_explorer", {path: src}).catch((e) =>
            console.error("open source dir failed:", e)
        );
    });

    document.getElementById("skill-import-select-all")?.addEventListener("click", () => {
        document.querySelectorAll(".skill-import-select").forEach((cb) => (cb.checked = true));
        updateImportConfirmState();
    });
    document.getElementById("skill-import-select-none")?.addEventListener("click", () => {
        document.querySelectorAll(".skill-import-select").forEach((cb) => (cb.checked = false));
        updateImportConfirmState();
    });

    document.getElementById("skill-import-skill-list")?.addEventListener("change", () => {
        updateImportConfirmState();
    });

    document.getElementById("skill-import-confirm")?.addEventListener("click", () => doSkillImportSelected());

    document.getElementById("skill-import-cancel")?.addEventListener("click", closeSkillImportPanel);
    document.getElementById("skill-import-overlay")?.addEventListener("click", (e) => {
        if (e.target.id === "skill-import-overlay") closeSkillImportPanel();
    });
}

/** 关闭导入面板并清理状态。 */
function closeSkillImportPanel() {
    const overlay = document.getElementById("skill-import-overlay");
    if (overlay) overlay.classList.add('hidden');
    const errorEl = document.getElementById("skill-import-error");
    if (errorEl) errorEl.textContent = "";
}

/** 渲染当前来源下的 skill 勾选列表。source 为 null 时显示空提示。 */
function renderSkillImportList(source) {
    const listEl = document.getElementById("skill-import-skill-list");
    if (!listEl) return;
    if (!source || !source.skills || source.skills.length === 0) {
        listEl.innerHTML = source && source.exists
            ? '<span class="skill-empty-hint">该目录下未发现 SKILL.md（每个子目录需含 SKILL.md）</span>'
            : '<span class="skill-empty-hint">请先选择来源</span>';
        updateImportConfirmState();
        return;
    }
    listEl.innerHTML = source.skills
        .map(
            (s, i) => `
      <label class="skill-import-item" title="${escapeAttr(s.dir)}">
        <input type="checkbox" class="skill-import-select" data-index="${i}" checked />
        <span class="skill-import-item-name">${escapeHtml(s.name)}</span>
        <span class="skill-import-item-desc">${escapeHtml(s.description)}</span>
      </label>`
        )
        .join("");
    updateImportConfirmState();
}

/** 更新来源目录信息提示。 */
function updateSkillImportSourceInfo(source) {
    const infoEl = document.getElementById("skill-import-source-info");
    if (!infoEl) return;
    if (!source) {
        infoEl.textContent = "";
        return;
    }
    infoEl.textContent = source.exists
        ? `目录: ${source.dir}`
        : `目录不存在: ${source.dir}（可点击右侧按钮创建）`;
}

/** 返回当前选中来源的目录路径。 */
function currentImportSourceDir() {
    const select = document.getElementById("skill-import-source-app");
    if (!select) return null;
    const val = select.value;
    if (val === "__custom__") {
        return aiState._customImportDir;
    }
    const src = (aiState._skillImportSourcesCache || []).find((s) => s.id === val);
    return src ? src.dir : null;
}

/** 根据勾选数量启用/禁用「导入选中」按钮。 */
function updateImportConfirmState() {
    const btn = document.getElementById("skill-import-confirm");
    if (!btn) return;
    const checked = document.querySelectorAll(".skill-import-select:checked").length;
    btn.disabled = checked === 0;
    btn.textContent = checked > 0 ? `导入选中 (${checked})` : "导入选中";
}

/** 执行导入：遍历勾选的 skill，逐个调 import_skill。 */
async function doSkillImportSelected() {
    const errorEl = document.getElementById("skill-import-error");
    if (errorEl) errorEl.textContent = "";
    const select = document.getElementById("skill-import-source-app");
    const val = select && select.value;
    let source = null;
    if (val === "__custom__") {
        source = {
            id: "__custom__",
            label: "自定义",
            dir: aiState._customImportDir || "",
            exists: true,
            skills: aiState._customImportDir
                ? [{
                    name: (aiState._customImportDir.split(/[\\/]/).pop() || aiState._customImportDir),
                    description: "（自定义目录）",
                    dir: aiState._customImportDir,
                }]
                : [],
        };
    } else {
        source = (aiState._skillImportSourcesCache || []).find((s) => s.id === val) || null;
    }
    if (!source) return;

    const selectedIndices = Array.from(
        document.querySelectorAll(".skill-import-select:checked")
    ).map((cb) => parseInt(cb.dataset.index, 10));
    const targets = selectedIndices.map((i) => source.skills[i]).filter(Boolean);
    if (targets.length === 0) return;

    const modeEl = document.querySelector('input[name="skill-import-mode"]:checked');
    const mode = modeEl ? modeEl.value : "copy";

    const btn = document.getElementById("skill-import-confirm");
    if (btn) {
        btn.disabled = true;
        btn.textContent = "导入中...";
    }

    const ok = [];
    const fail = [];
    for (const s of targets) {
        try {
            await invoke("import_skill", {sourcePath: s.dir, mode});
            ok.push(s.name);
        } catch (e) {
            fail.push(`${s.name}: ${e}`);
            console.error("import_skill failed:", s.name, e);
        }
    }

    try {
        await invoke("refresh_skills");
        await loadSkillList();
    } catch (e) {
        console.error("refresh after import failed:", e);
    }

    closeSkillImportPanel();

    const hint = document.getElementById("skill-import-hint");
    if (hint) {
        let msg = `导入完成：成功 ${ok.length} 个`;
        if (fail.length) msg += `，失败 ${fail.length} 个（${fail.join("; ")}）`;
        hint.textContent = msg;
        hint.classList.remove('hidden');
        setTimeout(() => {
            hint.classList.add('hidden');
        }, 6000);
    }
}

/**
 * 扫描自定义目录下的 skill 子目录（前端轻量实现，仅展示用）。
 */
async function scanCustomDirForSkills(dir) {
    const name = dir.split(/[\\/]/).pop() || dir;
    return [{name, description: "（自定义目录）", dir}];
}

// ── Skill 编辑 modal ───────────────────────────────────────

/**
 * 显示 Skill 编辑 modal。
 */
export function showSkillEditModal(skillDir, content, cliPath) {
    const overlay = document.getElementById("skill-edit-overlay");
    if (!overlay) return;
    overlay.dataset.skillDir = skillDir;
    const textarea = document.getElementById("skill-edit-textarea");
    if (textarea) textarea.value = content;
    const titleEl = document.getElementById("skill-edit-title");
    if (titleEl) titleEl.textContent = cliPath ? `编辑 Skill（来自 ${cliPath}）` : "编辑 Skill";
    const metaEl = document.getElementById("skill-edit-meta");
    if (metaEl) metaEl.textContent = `目录: ${skillDir}`;
    const errorEl = document.getElementById("skill-edit-error");
    if (errorEl) errorEl.textContent = "";
    overlay.classList.remove('hidden');
}

// ── CLI 能力识别（IIFE，模块加载时绑定）────────────────────

(() => {
    const $ = (id) => document.getElementById(id);

    $("cli-recognizer-browse")?.addEventListener("click", async () => {
        try {
            const selected = await invoke("open_file_dialog", {
                title: "选择 CLI 可执行文件",
                filters: [{name: "可执行文件", extensions: ["exe", "cmd", "bat"]}],
            });
            if (selected) {
                const input = $("cli-recognizer-input");
                if (input) input.value = selected;
            }
        } catch (e) {
            console.error("browse cli file failed:", e);
        }
    });

    $("cli-recognizer-btn")?.addEventListener("click", async () => {
        const input = $("cli-recognizer-input");
        if (!input) return;
        const cliPath = input.value.trim();
        if (!cliPath) {
            input.focus();
            return;
        }

        const errorEl = $("cli-recognizer-error");
        const btn = $("cli-recognizer-btn");
        if (errorEl) errorEl.textContent = "";
        if (btn) {
            btn.disabled = true;
            btn.textContent = "识别中…";
        }

        try {
            const result = await invoke("recognize_cli_tool", {cliPath});
            const skillDir = result.saved_path.replace(/SKILL\.md$/, "").replace(/[/\\]$/, "");
            showSkillEditModal(skillDir, result.skill_md_content, result.source_cli_path || cliPath);
            await invoke("refresh_skills").catch(() => {
            });
            await loadSkillList();
        } catch (e) {
            if (errorEl) errorEl.textContent = String(e);
            console.error("recognize_cli_tool failed:", e);
        } finally {
            if (btn) {
                btn.disabled = false;
                btn.textContent = "识别";
            }
        }
    });

    $("cli-recognizer-input")?.addEventListener("keydown", (e) => {
        if (e.key === "Enter") {
            e.preventDefault();
            $("cli-recognizer-btn")?.click();
        }
    });
})();
