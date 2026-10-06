/**
 * 关于 Tab 模块
 * 包含：版本信息、许可、源码仓库、检查更新
 *
 * 0.9.5 拆分时误用 get_about_info（后端无此命令，报 not found）+ 字段名
 * (tauri_version/webview_version) 与后端 get_app_info 返回不匹配；0.9.5.1 还原原版。
 * 0.17.1：新增 release notes 展示（Markdown 渲染）
 * 0.25.10：新增一键更新（invoke install_update + UPDATE_INSTALL_PROGRESS 进度事件）
 */
import {invoke, listen, normalizeError} from "../../shared/tauri.js";
import {EVENTS} from "../../shared/event-names.js";
import {initMarkdown, renderMarkdown} from "../../shared/markdown.js";
import {t} from "../../i18n/index.js";

/**
 * 初始化关于 Tab
 */
export function initAboutTab() {
    // 初始化 Markdown 渲染器（vendor 脚本在 settings.html 底部加载）
    initMarkdown();
    loadAboutInfo();
    initCheckUpdate();
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
 * 检查更新按钮 + 一键更新（0.25.10）
 *
 * 两条链路：
 * - 检查：invoke check_update → 展示新版本 / notes / 「一键更新」入口
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

    btn.addEventListener("click", async () => {
        btn.disabled = true;
        updateEl.hidden = false;
        updateEl.textContent = "…";
        // 重置 release notes 区域
        if (notesEl) {
            notesEl.hidden = true;
            notesEl.innerHTML = "";
        }
        if (installBtn) installBtn.hidden = true;
        if (progressEl) progressEl.hidden = true;
        try {
            const r = await invoke("check_update");
            if (r.error) {
                // 网络失败 / API 异常 —— 显示后端返回的具体原因，方便用户判断
                // 常见场景：403 = GitHub 匿名限流（60 次/小时），「网络」= 代理/断网
                updateEl.textContent = friendlyUpdateError(r.error);
            } else if (r.has_update) {
                // .external-link + data-url 走统一外链委托（外部浏览器打开）
                // .about-update-link 套用项目统一超链样式（accent 色）
                const link = r.release_url
                    ? ` · <a href="#" class="external-link about-update-link" data-url="${r.release_url}">查看</a>`
                    : "";
                updateEl.innerHTML = `新版本 ${r.latest_version} 可用${link}`;
                // 0.25.10：发现新版本 → 露出一键更新入口
                if (installBtn) installBtn.hidden = false;
                // 0.17.1：展示 release notes（Markdown 渲染）
                if (notesEl) {
                    const notes = r.release_notes || "";
                    if (notes.trim()) {
                        renderMarkdown(notes, {container: notesEl});
                        notesEl.hidden = false;
                    } else {
                        notesEl.textContent = t("about.update.no_notes");
                        notesEl.hidden = false;
                    }
                }
            } else {
                updateEl.textContent = `已是最新版本（${r.current_version}）`;
            }
        } catch (e) {
            updateEl.textContent = "检查失败";
            console.error("check_update failed:", e);
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
