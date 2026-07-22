/**
 * 本地模型 Tab 模块（0.12 §2.7 新增）
 *
 * 功能：
 * 1. ollama 探测：连接 localhost:11434/api/tags，展示本地模型列表
 * 2. 一键添加到 AI 供应商：将 ollama 模型加入 AIConfig providers
 * 3. LM Studio 帮助文案：五步图文引导
 */
import { invoke } from "../../tauri.js";
import { t, onLangChange } from "../../i18n/index.js";
import { saveConfig } from "../../config-keys.js";

/** 初始化本地模型 Tab */
export async function initLocalModelTab() {
  const panel = document.getElementById("local-model");
  if (!panel) return;

  // ── ollama 探测 ──
  const statusBadge = document.getElementById("ollama-status-badge");
  const probeBtn = document.getElementById("ollama-probe-btn");
  const baseUrlInput = document.getElementById("ollama-base-url");
  const modelsContainer = document.getElementById("ollama-models-container");

  let _ollamaModels = []; // 缓存最近一次拉取的模型列表

  async function probeOllama() {
    const baseUrl = baseUrlInput?.value?.trim() || "http://localhost:11434";
    if (statusBadge) {
      statusBadge.textContent = t("local_model.ollama.probing");
      statusBadge.className = "status-badge status-unknown";
    }
    if (probeBtn) probeBtn.disabled = true;

    try {
      const models = await invoke("fetch_ai_models", {
        kind: "ollama_http",
        baseUrl,
        apiKey: null,
        providerId: null,
      });

      _ollamaModels = Array.isArray(models) ? models : [];

      if (statusBadge) {
        statusBadge.textContent = t("local_model.ollama.connected", { count: _ollamaModels.length });
        statusBadge.className = "status-badge status-ok";
      }
      renderOllamaModels(_ollamaModels);
    } catch (e) {
      console.error("ollama probe failed:", e);
      if (statusBadge) {
        statusBadge.textContent = t("local_model.ollama.not_connected");
        statusBadge.className = "status-badge status-error";
      }
      if (modelsContainer) {
        modelsContainer.innerHTML = `<p class="setting-hint">${escapeHtml(t("local_model.ollama.not_connected_hint"))}</p>`;
      }
    } finally {
      if (probeBtn) probeBtn.disabled = false;
    }
  }

  function renderOllamaModels(models) {
    if (!modelsContainer) return;
    if (models.length === 0) {
      modelsContainer.innerHTML = `<p class="setting-hint">${escapeHtml(t("local_model.ollama.no_models"))}</p>`;
      return;
    }

    const rows = models.map((m) => {
      const isEmbedding = /embed|nomic|minilm/i.test(m);
      const caps = isEmbedding ? ["embedding"] : ["chat"];
      const capsHtml = caps.map((cap) =>
        `<span class="ai-cap-badge ai-cap-${escapeAttr(cap)}">${escapeHtml(t("ai.cap." + cap))}</span>`
      ).join("");

      return `<div class="ollama-model-row" data-model-id="${escapeAttr(m)}">
        <span class="ollama-model-name">${escapeHtml(m)}</span>
        <span class="ollama-model-caps">${capsHtml}</span>
        <button class="btn-small ollama-model-add-btn" data-model-id="${escapeAttr(m)}" data-caps="${escapeAttr(caps.join(","))}">
          ${escapeHtml(t("local_model.ollama.add_to_ai"))}
        </button>
      </div>`;
    }).join("");

    modelsContainer.innerHTML = `<div class="ollama-models-list">${rows}</div>`;

    modelsContainer.querySelectorAll(".ollama-model-add-btn").forEach((btn) => {
      btn.addEventListener("click", async () => {
        const modelId = btn.dataset.modelId;
        const caps = btn.dataset.caps?.split(",") || ["chat"];
        await addOllamaModelToAI(modelId, caps);
      });
    });
  }

  /** 将 ollama 模型添加到 AIConfig providers */
  async function addOllamaModelToAI(modelId, capabilities) {
    let aiConfig;
    try {
      aiConfig = await invoke("get_config_section", { key: "app.ai" });
      if (!aiConfig || typeof aiConfig !== "object") aiConfig = { providers: [] };
    } catch (e) {
      console.error("get_config_section app.ai failed:", e);
      aiConfig = { providers: [] };
    }

    const providers = aiConfig.providers || [];
    const baseUrl = baseUrlInput?.value?.trim() || "http://localhost:11434";

    // 查找已有的 ollama provider（按 base_url 精确匹配）
    let ollamaProvider = providers.find((p) =>
      p.kind === "ollama_http" && (p.base_url || "").replace(/\/+$/, "") === baseUrl.replace(/\/+$/, "")
    );

    if (!ollamaProvider) {
      // 创建新的 ollama provider
      const providerId = (crypto.randomUUID && crypto.randomUUID()) || `p-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
      ollamaProvider = {
        id: providerId,
        display_name: "Ollama",
        kind: "ollama_http",
        base_url: baseUrl,
        secret_ref: `blink/${providerId}/key`,
        models: [],
        created_at: Math.floor(Date.now() / 1000),
      };
      providers.push(ollamaProvider);
    }

    // 检查模型是否已存在
    const existingModel = (ollamaProvider.models || []).find((m) => m.id === modelId);
    if (existingModel) {
      // 更新能力
      existingModel.capabilities = capabilities;
    } else {
      ollamaProvider.models = ollamaProvider.models || [];
      ollamaProvider.models.push({
        id: modelId,
        display_name: modelId,
        enabled: true,
        context_window: null,
        input_price_per_million: null,
        output_price_per_million: null,
        temperature: null,
        max_tokens: null,
        custom_parameters: [],
        capabilities,
      });
    }

    aiConfig.providers = providers;

    try {
      await saveConfig("ai_config", aiConfig);
      // 标记按钮为已添加
      const btn = modelsContainer?.querySelector(`.ollama-model-add-btn[data-model-id="${CSS.escape(modelId)}"]`);
      if (btn) {
        btn.textContent = "✓";
        btn.disabled = true;
        btn.classList.add("added");
      }
    } catch (e) {
      console.error("save ai_config failed:", e);
      alert(t("local_model.ollama.add_failed", { err: String(e) }));
    }
  }

  // 初始探测
  await probeOllama();

  // 探测按钮
  if (probeBtn) {
    probeBtn.addEventListener("click", probeOllama);
  }

  // base_url 变更时不自动探测（等用户点按钮）

  // ── LM Studio 帮助 ──
  const lmstudioGotoAiBtn = document.getElementById("lmstudio-goto-ai-btn");
  if (lmstudioGotoAiBtn) {
    lmstudioGotoAiBtn.addEventListener("click", () => {
      document.querySelector('.tab[data-tab="ai"]')?.click();
    });
  }

  // 语言切换时重新渲染
  onLangChange(() => {
    if (_ollamaModels.length > 0) {
      renderOllamaModels(_ollamaModels);
    }
  });
}

// ── helper ────────────────────────────────────────────────────────────

function escapeHtml(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function escapeAttr(s) {
  return escapeHtml(s);
}
