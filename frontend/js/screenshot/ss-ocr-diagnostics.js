//! OCR 诊断面板（0.17.5）。默认关闭，开启后工具栏显示诊断按钮。
//!
//! 开关：设置页 Chord → 截图 → OCR 诊断（持久化到 SQLite 配置库）；
//! URL `?ocrDebug=1` 可一次性覆盖（dev 调试用）。
//! 面板展示已安装 OCR 语言列表、中文包状态、引擎语言、选区 OCR 测试结果。
//! 无中文包时显示安装引导。

import { ocrDiagnose, ocrImage } from "../shared/api.js";
import { t } from "../i18n/index.js";
import { ss } from "./ss-state.js";
import { compositeSelection } from "./ss-output.js";

const PANEL_ID = "ocr-diagnostics-panel";

/** 诊断开关是否开启（URL 覆盖 > 配置库 > localStorage 兼容旧版）。 */
export function ocrDiagnosticsEnabled() {
  // URL 覆盖：dev 一次性调试用，优先级最高
  if (new URLSearchParams(window.location.search).get("ocrDebug") === "1") return true;
  // 从 SQLite 配置库缓存读取（index.js loadScreenshot 时异步写入 ss.screenshotConfig）
  if (ss.screenshotConfig.ocrDebug === true) return true;
  // 兼容旧版 localStorage
  return localStorage.getItem("blink.ocrDebug") === "1";
}

/** 配置读完后刷新诊断按钮可见性（index.js 启动时调用）。 */
export function refreshOcrDiagnosticsVisibility() {
  const btn = document.getElementById("btn-ocr-diag");
  if (btn) btn.hidden = !ocrDiagnosticsEnabled();
}

/** 诊断主入口：创建面板 → 查询环境信息 → 选区 OCR 测试。 */
export async function doOcrDiagnostics() {
  let panel = document.getElementById(PANEL_ID);
  if (panel) panel.remove();
  panel = createPanel();
  document.body.appendChild(panel);

  const langsEl = panel.querySelector("#ocr-diag-langs");
  const chineseEl = panel.querySelector("#ocr-diag-chinese");
  const engineEl = panel.querySelector("#ocr-diag-engine");
  const testEl = panel.querySelector("#ocr-diag-test");
  const guideEl = panel.querySelector("#ocr-diag-install-guide");

  // 1. 环境信息
  langsEl.textContent = t("screenshot.ocr_diag.loading");
  try {
    const diag = await ocrDiagnose();
    const langs = diag.available_languages || [];
    langsEl.textContent = langs.length ? langs.join(", ") : "—";

    const hasChinese = diag.has_chinese === true;
    chineseEl.textContent = hasChinese
      ? "✅ " + t("screenshot.ocr_diag.installed")
      : "❌ " + t("screenshot.ocr_diag.not_installed");

    const engineLang = diag.engine_language;
    engineEl.textContent = engineLang || "— (fallback)";

    if (!hasChinese) {
      guideEl.classList.remove("hidden");
      guideEl.querySelector("button").addEventListener("click", () => {
        try {
          open("ms-settings:regionlanguage", "_blank");
        } catch {
          // fallback：提示手动打开
          showText(testEl, t("screenshot.ocr_diag.install_guide"));
        }
      });
    }
  } catch (err) {
    langsEl.textContent = "❌ " + (err?.message || String(err));
  }

  // 2. 选区 OCR 测试
  if (!ss.selCss) {
    testEl.textContent = t("screenshot.ocr_diag.no_selection");
    return;
  }

  testEl.textContent = t("screenshot.ocr_diag.testing");
  try {
    const pngBytes = await new Promise((resolve) => {
      compositeSelection((bytes) => resolve(bytes));
    });
    if (!pngBytes) {
      testEl.textContent = "❌ " + t("screenshot.ocr_diag.test_result") + ": compositeSelection empty";
      return;
    }

    const startTs = performance.now();
    const result = await ocrImage(pngBytes);
    const elapsed = Math.round(performance.now() - startTs);

    const lines = result?.lines?.length || 0;
    const chars = result?.text?.length || 0;
    const preview = (result?.text || "").slice(0, 80);

    testEl.innerHTML = formatTestResult(lines, chars, elapsed, preview);
  } catch (err) {
    testEl.textContent = "❌ " + (err?.message || String(err));
  }
}

// ── 内部 ────────────────────────────────────────────────────────────────────

function createPanel() {
  const panel = document.createElement("div");
  panel.id = PANEL_ID;
  panel.className = "ocr-diagnostics-panel";
  panel.innerHTML = `
    <div class="ocr-diag-header">
      <span class="ocr-diag-title">${t("screenshot.ocr_diag.title")}</span>
      <button class="ocr-diag-close" title="${t("screenshot.ocr_diag.close")}">
        <svg class="icon" aria-hidden="true"><use href="#icon-x"/></svg>
      </button>
    </div>
    <div class="ocr-diag-body">
      <div class="ocr-diag-section">
        <span class="ocr-diag-label">${t("screenshot.ocr_diag.available_langs")}</span>
        <span class="ocr-diag-value" id="ocr-diag-langs">…</span>
      </div>
      <div class="ocr-diag-section">
        <span class="ocr-diag-label">${t("screenshot.ocr_diag.has_chinese")}</span>
        <span class="ocr-diag-value" id="ocr-diag-chinese">…</span>
      </div>
      <div class="ocr-diag-section">
        <span class="ocr-diag-label">${t("screenshot.ocr_diag.engine_lang")}</span>
        <span class="ocr-diag-value" id="ocr-diag-engine">…</span>
      </div>
      <div class="ocr-diag-section">
        <span class="ocr-diag-label">${t("screenshot.ocr_diag.test_result")}</span>
        <span class="ocr-diag-value" id="ocr-diag-test">…</span>
      </div>
      <div class="ocr-diag-install-guide hidden" id="ocr-diag-install-guide">
        <p class="ocr-diag-hint">${t("screenshot.ocr_diag.no_chinese_pack")}</p>
        <button class="ocr-diag-open-settings">${t("screenshot.ocr_diag.install_guide")}</button>
      </div>
    </div>
  `;
  panel.querySelector(".ocr-diag-close").addEventListener("click", () => panel.remove());
  return panel;
}

function formatTestResult(lines, chars, elapsedMs, preview) {
  const parts = [
    `${t("screenshot.ocr_diag.lines")}: ${lines}`,
    `${t("screenshot.ocr_diag.chars")}: ${chars}`,
    `${t("screenshot.ocr_diag.elapsed")}: ${elapsedMs}ms`,
  ];
  let html = parts.join(" · ");
  if (preview) {
    html += `<br><span class="ocr-diag-preview">${escapeHtml(preview)}</span>`;
  }
  return html;
}

function showText(el, text) {
  if (el) el.textContent = text;
}

function escapeHtml(str) {
  const div = document.createElement("div");
  div.textContent = str;
  return div.innerHTML;
}
