/**
 * 关于 Tab 模块
 * 包含：版本信息、许可、源码仓库、检查更新
 *
 * 0.9.5 拆分时误用 get_about_info（后端无此命令，报 not found）+ 字段名
 * (tauri_version/webview_version) 与后端 get_app_info 返回不匹配；0.9.5.1 还原原版。
 */
import { invoke } from "../../tauri.js";

/**
 * 初始化关于 Tab
 */
export function initAboutTab() {
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
 * 检查更新按钮
 */
function initCheckUpdate() {
  const btn = document.getElementById("about-check-update");
  const updateEl = document.getElementById("about-update");
  if (!btn || !updateEl) return;

  btn.addEventListener("click", async () => {
    btn.disabled = true;
    updateEl.hidden = false;
    updateEl.textContent = "…";
    try {
      const r = await invoke("check_update");
      if (r.error) {
        // 网络失败 / API 异常 —— 区分「真没更新」和「请求失败了」
        updateEl.textContent = "检查失败，请稍后重试";
      } else if (r.has_update) {
        // .external-link + data-url 走统一外链委托（外部浏览器打开）
        // .about-update-link 套用项目统一超链样式（accent 色）
        const link = r.release_url
          ? ` · <a href="#" class="external-link about-update-link" data-url="${r.release_url}">查看</a>`
          : "";
        updateEl.innerHTML = `新版本 ${r.latest_version} 可用${link}`;
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
}
