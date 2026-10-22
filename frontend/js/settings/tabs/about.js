/**
 * 关于 Tab 模块
 * 包含：版本信息、许可、源码仓库、检查更新
 *
 * 0.9.5 拆分时误用 get_about_info（后端无此命令，报 not found）+ 字段名
 * (tauri_version/webview_version) 与后端 get_app_info 返回不匹配；0.9.5.1 还原原版。
 * 0.17.1：新增 release notes 展示（Markdown 渲染）
 * 0.25.10：新增一键更新（invoke install_update + UPDATE_INSTALL_PROGRESS 进度事件）
 * 0.25.19：更新日志多版本分组展示（跳版升级时从新到旧列出全部待更新版本）
 * 0.26.19：新增「开源与许可」声明入口（get_license_notices，含托管 Everything 官方 License.txt 全文）
 */
import {invoke, listen, normalizeError} from "../../shared/tauri.js";
import {EVENTS} from "../../shared/event-names.js";
import {initMarkdown, renderMarkdown} from "../../shared/markdown.js";
import {t} from "../../i18n/index.js";
import {navigateSettings} from "../navigation.js";
import {saveConfig} from "../../shared/config-keys.js";
import {initDeveloperMode} from '../developer-mode.js';

/**
 * 初始化关于 Tab
 */
export function initAboutTab() {
    initDeveloperMode();
    // 初始化 Markdown 渲染器（vendor 脚本在 settings.html 底部加载）
    initMarkdown();
    loadAboutInfo();
    initCheckUpdate();
    initLicenseNotices();

    // 丝滑小妙招入口（0.25.28）：跨 tab 跳转统一走 navigation（spec-frontend §5.7）
    document.getElementById("about-guide-open")?.addEventListener("click", () => {
        navigateSettings({tabId: "guide"}).catch((e) => console.error("navigate to guide failed:", e));
    });
}

/**
 * 开源与许可声明（0.26.19）
 *
 * 托管 Everything 属随应用再分发，MIT 要求副本附许可声明。点击「查看声明」
 * 按需拉取后端聚合的声明列表（官方 License.txt 全文 + 落地路径），再点收起。
 * 数据静态且一次性渲染，无版本竞态；加载失败保持可重试。
 */
function initLicenseNotices() {
    const btn = document.getElementById("about-licenses-open");
    const panel = document.getElementById("about-licenses");
    if (!btn || !panel) return;

    btn.addEventListener("click", async () => {
        if (!panel.hidden) {
            panel.hidden = true;
            return;
        }
        if (!panel.dataset.loaded) {
            try {
                const notices = await invoke("get_license_notices");
                renderLicenseNotices(panel, notices);
                panel.dataset.loaded = "1";
            } catch (e) {
                console.error("get_license_notices failed:", e);
                panel.textContent = t("about.license.load_failed");
                // 不标记 loaded，下次点击自动重试
            }
        }
        panel.hidden = false;
    });
}

/**
 * 渲染声明条目列表。
 * 结构与后端 LicenseNotice 对齐；installed_path 为磁盘上的许可文件位置
 * （mono 展示合理），声明正文用 pre 保持原文排版、正文断行。
 */
function renderLicenseNotices(panel, notices) {
    panel.innerHTML = "";
    if (!Array.isArray(notices) || !notices.length) {
        panel.textContent = t("about.license.load_failed");
        return;
    }
    for (const n of notices) {
        const item = document.createElement("div");
        item.className = "about-license-item";

        const head = document.createElement("div");
        head.className = "about-license-head";
        const name = document.createElement("span");
        name.className = "about-license-name";
        name.textContent = n.name || n.id;
        head.appendChild(name);
        if (n.version) {
            const ver = document.createElement("span");
            ver.className = "about-license-version";
            ver.textContent = n.version;
            head.appendChild(ver);
        }
        const kind = document.createElement("span");
        kind.className = "about-license-kind";
        kind.textContent = n.license || "";
        head.appendChild(kind);
        if (n.homepage) {
            // .external-link + data-url 走统一外链委托（外部浏览器打开）
            const link = document.createElement("a");
            link.href = "#";
            link.className = "external-link about-license-homepage";
            link.dataset.url = n.homepage;
            link.textContent = t("about.license.homepage");
            head.appendChild(link);
        }
        item.appendChild(head);

        const path = document.createElement("div");
        path.className = "about-license-path";
        if (n.installed_path) {
            const label = document.createElement("span");
            label.className = "about-license-path-label";
            label.textContent = t("about.license.installed_at");
            const code = document.createElement("code");
            code.textContent = n.installed_path;
            path.appendChild(label);
            path.appendChild(code);
        } else {
            path.textContent = t("about.license.not_installed");
        }
        item.appendChild(path);

        const body = document.createElement("pre");
        body.className = "about-license-body";
        body.textContent = n.body || "";
        item.appendChild(body);

        panel.appendChild(item);
    }
}

/**
 * 加载关于信息（版本 / 许可 / 仓库）
 */
async function loadAboutInfo() {
    try {
        const info = await invoke("get_app_info");
        const versionEl = document.getElementById("about-version");
        if (versionEl) versionEl.textContent = info.version || "—";
        const licenseEl = document.getElementById("about-license");
        if (licenseEl) licenseEl.textContent = info.license || "—";
        const repoEl = document.getElementById("about-repository");
        if (repoEl) {
            const url = info.repository || "";
            repoEl.textContent = url || "—";
            // 用 data-url 而非 href，走统一的 .external-link 事件委托（外部浏览器打开）
            if (url) repoEl.dataset.url = url;
        }
    } catch (e) {
        console.error("loadAboutInfo failed:", e);
    }
}

/**
 * 检查更新按钮 + 一键更新 + 打开即自动检查 + 自动检查开关（0.26.20）
 *
 * 三条链路：
 * - 手动检查：点击按钮 → invoke check_update → 展示新版本 / notes / 「一键更新」入口
 * - 自动检查（0.26.20）：关于 tab 每次激活静默执行一次（≥60s 节流防快速切换
 *   tab 反复打 GitHub 匿名限流；「自动检查更新」关闭时不自动跑），结果渲染
 *   与手动按钮共用同一函数
 * - 安装：invoke install_update 立即返回 started，之后进度只走
 *   UPDATE_INSTALL_PROGRESS 事件（downloading → installing → 应用被
 *   安装器接管自动重启，即成功终点）；检查阶段失败走 invoke 错误，
 *   下载阶段失败走 failed 事件。
 */
function initCheckUpdate() {
    const btn = document.getElementById("about-check-update");
    const updateEl = document.getElementById("about-update");
    const notesEl = document.getElementById("about-release-notes");
    const installBtn = document.getElementById("about-install-update");
    const progressEl = document.getElementById("about-update-progress");
    const progressBar = document.getElementById("about-update-progress-bar");
    if (!btn || !updateEl) return;

    // 事件监听在初始化注册一次（settings 窗口常驻）；百分比只更新数值文案，
    // 阶段文案由 stage 驱动，参照本地引擎「阶段/进度分离」惯例但单事件承载
    listen(EVENTS.UPDATE_INSTALL_PROGRESS, (event) => {
        const p = event?.payload || {};
        if (p.stage === "downloading") {
            progressEl.hidden = false;
            if (p.total) {
                const pct = Math.min(100, Math.round((p.downloaded / p.total) * 100));
                progressBar.style.width = `${pct}%`;
                progressBar.classList.remove("about-update-progress-indeterminate");
                updateEl.textContent = t("about.update.downloading", {percent: pct});
            } else {
                // total 未知：不确定态动画 + 已下载字节数
                progressBar.style.width = "100%";
                progressBar.classList.add("about-update-progress-indeterminate");
                updateEl.textContent = t("about.update.downloading_no_size", {
                    downloaded: formatBytes(p.downloaded),
                });
            }
        } else if (p.stage === "installing") {
            progressEl.hidden = false;
            progressBar.style.width = "100%";
            progressBar.classList.remove("about-update-progress-indeterminate");
            updateEl.textContent = t("about.update.installing");
        } else if (p.stage === "failed") {
            progressEl.hidden = true;
            updateEl.textContent = friendlyUpdateError(p.error, "更新失败");
            installBtn.hidden = false;
            installBtn.disabled = false;
        }
    });

    /**
     * 执行一次检查并渲染结果（手动按钮与 tab 激活自动检查共用渲染函数
     * `renderCheckResult`；此处负责按钮态与区域重置）。
     * @returns {Promise<void>}
     */
    async function runCheck() {
        updateEl.hidden = false;
        updateEl.textContent = "…";
        if (notesEl) {
            notesEl.hidden = true;
            notesEl.innerHTML = "";
        }
        if (installBtn) installBtn.hidden = true;
        if (progressEl) progressEl.hidden = true;
        try {
            renderCheckResult(await invoke("check_update"));
        } catch (e) {
            updateEl.textContent = "检查失败";
            console.error("check_update failed:", e);
        }
    }

    btn.addEventListener("click", async () => {
        btn.disabled = true;
        try {
            await runCheck();
        } finally {
            btn.disabled = false;
        }
    });

    if (installBtn) {
        installBtn.addEventListener("click", async () => {
            installBtn.disabled = true;
            updateEl.hidden = false;
            updateEl.textContent = t("about.update.preparing");
            if (progressEl) progressEl.hidden = true;
            try {
                const r = await invoke("install_update");
                if (r.status === "no_update") {
                    // 检查展示与点击之间版本状态可能变化（如已装新版）
                    updateEl.textContent = t("about.update.latest");
                    installBtn.hidden = true;
                } else {
                    // started —— 进度条归零，后续状态全部由事件驱动
                    if (progressEl) {
                        progressEl.hidden = false;
                        progressBar.style.width = "0%";
                    }
                    // 应用即将被安装器接管并自动重启；按钮保持 disabled 防误触
                }
            } catch (e) {
                const err = normalizeError(e);
                updateEl.textContent = friendlyUpdateError(err.message, "更新失败");
                installBtn.disabled = false;
                console.error("install_update failed:", e);
            }
        });
    }

    initAutoCheckToggle();
}

// ── 0.26.20：关于 tab 激活自动检查 + 「自动检查更新」开关 ──────────────────

/** tab 激活自动检查的最小间隔（防快速切换反复打 GitHub 匿名限流）。 */
const AUTO_CHECK_MIN_INTERVAL_MS = 60 * 1000;
let lastAutoCheckAt = 0;

/**
 * 关于 tab 激活钩子（settings/index.js tab 切换回调；设置页窗口预热后 init
 * 也会执行，故不能在 initAboutTab 里直接检查——那等于应用启动即查）。
 * 「自动检查更新」关闭时不自动跑（用户仍可点手动按钮）。
 */
export function onAboutTabActivated() {
    const toggle = document.getElementById("about-auto-check");
    if (toggle && !toggle.checked) return;
    const now = Date.now();
    if (now - lastAutoCheckAt < AUTO_CHECK_MIN_INTERVAL_MS) return;
    lastAutoCheckAt = now;
    invoke("check_update").then((r) => {
        renderCheckResult(r);
    }).catch((e) => {
        // 静默自动检查失败不打扰（与手动按钮的显式错误展示区分）
        console.debug("about tab auto check failed:", e);
    });
}

/**
 * 把 check_update 结果渲染到关于 tab（手动按钮与 tab 激活自动检查共用）。
 * 手动路径的按钮禁用/区域重置由各自调用方处理，此处只渲染结果；
 * 非新版路径收起 release notes（防上次检查的旧内容残留）。
 */
function renderCheckResult(r) {
    const updateEl = document.getElementById("about-update");
    const notesEl = document.getElementById("about-release-notes");
    const installBtn = document.getElementById("about-install-update");
    const progressEl = document.getElementById("about-update-progress");
    if (!updateEl) return;
    updateEl.hidden = false;
    // 0.27.4：镜像降级（主站不可达、镜像判出版本）时追加说明，如实标注
    const degradedHint = r.degraded ? t("about.update.degraded") : "";
    if (r.error) {
        updateEl.textContent = friendlyUpdateError(r.error);
    } else if (r.has_update) {
        // .external-link + data-url 走统一外链委托（外部浏览器打开）；
        // .about-update-link 套用项目统一超链样式（accent 色）
        const link = r.release_url
            ? ` · <a href="#" class="external-link about-update-link" data-url="${r.release_url}">查看</a>`
            : "";
        updateEl.innerHTML = `新版本 ${r.latest_version} 可用${degradedHint}${link}`;
        if (installBtn) installBtn.hidden = false;
        renderUpdateNotes(notesEl, r);
    } else {
        updateEl.textContent = `已是最新版本（${r.current_version}）${degradedHint}`;
    }
    if (!r.has_update) {
        if (notesEl) notesEl.hidden = true;
        if (installBtn) installBtn.hidden = true;
    }
    if (progressEl) progressEl.hidden = true;
}

/**
 * 「自动检查更新」开关（0.26.20）：读 app.update 分片回填，change 走
 * saveConfig("auto_check_update")（后端联动后台任务/建议槽/托盘提示与托盘重建）。
 */
function initAutoCheckToggle() {
    const toggle = document.getElementById("about-auto-check");
    if (!toggle) return;
    invoke("get_config_section", {key: "app.update"}).then((cfg) => {
        // 缺省（分片未写或后端 None）= 默认开
        toggle.checked = cfg?.auto_check_enabled !== false;
    }).catch((e) => console.error("read app.update failed:", e));

    toggle.addEventListener("change", async () => {
        const desired = toggle.checked;
        toggle.disabled = true;
        try {
            await saveConfig("auto_check_update", desired);
        } catch (err) {
            toggle.checked = !desired;
            console.error("set auto_check_update failed:", err);
        } finally {
            toggle.disabled = false;
        }
    });
}

/**
 * 渲染更新日志区（0.25.19 多版本分组）。
 *
 * r.updates 是后端筛好的待更新版本（从新到旧，已滤掉 release 模板段）：
 * - 单版本：维持 0.17.1 形态只渲染正文，版本号不重复展示（上方
 *   「新版本 x 可用」一行已示）
 * - 多版本（跳版升级）：每版一个分组——版本号标签 + 各自「查看」链接 + 正文
 * updates 缺失/为空时兜底单条 release_notes，再退到 no_notes 文案。
 */
function renderUpdateNotes(notesEl, r) {
    if (!notesEl) return;
    const updates = Array.isArray(r.updates) && r.updates.length
        ? r.updates
        : (r.release_notes || "").trim()
            ? [{version: r.latest_version, notes: r.release_notes, release_url: r.release_url}]
            : [];
    if (!updates.length) {
        notesEl.textContent = t("about.update.no_notes");
        notesEl.hidden = false;
        return;
    }
    notesEl.innerHTML = "";
    const multi = updates.length > 1;
    for (const u of updates) {
        const section = document.createElement("div");
        section.className = "release-notes-version";
        if (multi) {
            const head = document.createElement("div");
            head.className = "release-notes-version-head";
            const tag = document.createElement("span");
            tag.className = "release-notes-version-tag";
            tag.textContent = `v${u.version}`;
            head.appendChild(tag);
            if (u.release_url) {
                // .external-link + data-url 走统一外链委托（外部浏览器打开）
                const link = document.createElement("a");
                link.href = "#";
                link.className = "external-link about-update-link";
                link.dataset.url = u.release_url;
                link.textContent = "查看";
                head.appendChild(link);
            }
            section.appendChild(head);
        }
        const body = document.createElement("div");
        section.appendChild(body);
        renderMarkdown(u.notes || "", {container: body});
        notesEl.appendChild(section);
    }
    notesEl.hidden = false;
}

/**
 * 字节数转用户可读文本（进度 total 未知时的退化展示）。
 */
function formatBytes(bytes) {
    const n = Number(bytes) || 0;
    if (n >= 1048576) return `${(n / 1048576).toFixed(1)} MB`;
    if (n >= 1024) return `${(n / 1024).toFixed(1)} KB`;
    return `${n} B`;
}

/**
 * 把后端 check_update / install_update 的原始 error 字符串转成一句用户能理解的话。
 *
 * 后端返回形如：
 *   "GitHub API 返回 403 Forbidden"   —— 匿名限流（每小时 60 次）
 *   "GitHub API 返回 429 Too Many Requests"
 *   "网络请求失败: ..."                 —— 代理/断网/DNS
 *   "响应解析失败"                      —— 返回体不是 JSON
 *
 * 不识别的错误原样展示——后端措辞已经够清晰，遮遮掩掩反而难排查。
 * action 为动作前缀（检查失败 / 更新失败），随调用链路变化。
 */
function friendlyUpdateError(raw, action = "检查失败") {
    const s = String(raw || "");
    if (/403/.test(s)) return `${action}：GitHub 限流，请稍后重试`;
    if (/429/.test(s)) return `${action}：请求过于频繁，请稍后重试`;
    if (/网络/.test(s) || /Network|timeout|Timeout/i.test(s)) {
        return `${action}：网络异常，请检查代理或连接`;
    }
    if (/signature|Signature/.test(s)) return `${action}：更新包签名校验未通过，请重新检查或稍后重试`;
    return `${action}：${s}`;
}
