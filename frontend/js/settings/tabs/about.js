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
      if (url) repoEl.href = url;
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
      if (r.has_update) {
        const link = r.release_url
          ? ` · <a href="${r.release_url}" data-external>查看</a>`
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
