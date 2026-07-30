//! AI Tab tier 路由（0.14.6 §4.2 拆分）。
//!
//! 渲染 tier 下拉选项、降级提示、状态横幅。

import { aiState, escapeHtml, escapeAttr } from "./state.js";
import { t } from "../../../i18n/index.js";

export function renderAITierSelects() {
  const cfg = aiState.currentAIConfig;
  const providers = cfg.providers || [];
  const options = [`<option value="">${escapeHtml(t("ai.tier.unassigned"))}</option>`];
  providers.forEach((p) => {
    (p.models || []).filter((m) => m.enabled !== false).forEach((m) => {
      const val = `${p.id}::${m.id}`;
      const label = `${p.display_name} / ${m.display_name || m.id}`;
      options.push(`<option value="${escapeAttr(val)}">${escapeHtml(label)}</option>`);
    });
  });
  const html = options.join("");
  ["router", "light", "main"].forEach((tier) => {
    const sel = document.getElementById(`ai-tier-${tier}`);
    if (!sel) return;
    sel.innerHTML = html;
    const assign = cfg[`tier_${tier}`];
    sel.value = assign ? `${assign.provider_id}::${assign.model_id}` : "";
  });
  renderAITierDegrade();
}

export function renderAITierDegrade() {
  const cfg = aiState.currentAIConfig;
  const chain = { router: ["light", "main"], light: ["main"], main: [] };
  const isUsable = (a) => {
    if (!a) return false;
    const provider = (cfg.providers || []).find((p) => p.id === a.provider_id);
    if (!provider) return false;
    const model = (provider.models || []).find((m) => m.id === a.model_id);
    return !!model && model.enabled !== false;
  };
  const findAssignmentDetail = (a) => {
    const provider = (cfg.providers || []).find((p) => p.id === a.provider_id);
    const model = provider && (provider.models || []).find((m) => m.id === a.model_id);
    return { provider, model };
  };
  ["router", "light", "main"].forEach((tier) => {
    const el = document.getElementById(`ai-tier-${tier}-degrade`);
    if (!el) return;
    const assign = cfg[`tier_${tier}`];
    if (assign) {
      const { provider, model } = findAssignmentDetail(assign);
      if (!provider || !model) {
        el.textContent = t("ai.tier.no_provider");
        el.className = "ai-tier-degrade error";
      } else if (model.enabled === false) {
        el.textContent = t("ai.tier.model_disabled_warn", { model: model.id });
        el.className = "ai-tier-degrade error";
      } else {
        el.textContent = "";
        el.className = "ai-tier-degrade";
      }
      return;
    }
    let target = null;
    for (const next of chain[tier]) {
      const a = cfg[`tier_${next}`];
      if (!isUsable(a)) continue;
      const provider = (cfg.providers || []).find((p) => p.id === a.provider_id);
      target = { tier: next, label: provider.display_name };
      break;
    }
    if (target) {
      el.textContent = t("ai.tier.degrade_to", { tier: t(`ai.tier.${target.tier}`) });
      el.className = "ai-tier-degrade warning";
    } else {
      el.textContent = tier === "main" && cfg.enabled ? t("ai.tier.no_provider") : "";
      el.className = tier === "main" && cfg.enabled ? "ai-tier-degrade error" : "ai-tier-degrade";
    }
  });
}

export function renderAITierBanner() {
  const banner = document.getElementById("ai-tier-banner");
  if (!banner) return;
  const cfg = aiState.currentAIConfig;
  if (!cfg.enabled) {
    banner.classList.add('hidden');
    return;
  }
  const hasIssue = ["router", "light", "main"].some((tier) => {
    const assign = cfg[`tier_${tier}`];
    if (!assign) return tier !== "main";
    const provider = (cfg.providers || []).find((p) => p.id === assign.provider_id);
    const model = provider && (provider.models || []).find((m) => m.id === assign.model_id);
    return !provider || !model || model.enabled === false;
  });
  const mainAssign = cfg.tier_main;
  const mainMissing = !mainAssign || !(cfg.providers || []).some((p) =>
    p.id === mainAssign.provider_id &&
    (p.models || []).some((m) => m.id === mainAssign.model_id && m.enabled !== false),
  );
  if (!hasIssue && !mainMissing) {
    banner.classList.add('hidden');
    return;
  }
  banner.classList.remove('hidden');
  banner.textContent = mainMissing
    ? t("ai.tier.no_provider").replace(/^→\s*/, "")
    : t("ai.tier.degrade_to", { tier: t("ai.tier.main") });
}
