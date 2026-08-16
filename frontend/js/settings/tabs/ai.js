/**
 * AI Tab 模块（0.14.6 §4.2 重构：按 sub-domain 拆分为子模块）。
 *
 * 本文件是编排层——仅负责：
 * - re-export initAITab（供 settings/index.js 调用）
 * - 注册跨模块回调（打破 provider.js ↔ model-edit.js 的循环依赖）
 *
 * 拆分模块（均在 ./ai/ 子目录下）：
 * - ai/state.js      — 共享状态 + 常量 + helpers + saveAIConfig + fetchAvailableModelsFor
 * - ai/tier.js       — tier 路由（renderAITierSelects / renderAITierDegrade / renderAITierBanner）
 * - ai/model-edit.js — 模型编辑 modal + 拉取 popover（openAIModelEditModal / bindAIModelEditModalEvents 等）
 * - ai/skill.js      — Skill 列表 + 导入面板 + CLI 识别（loadSkillList / showSkillImportPanel 等）
 * - ai/provider.js   — 供应商渲染 + modal + preset 目录 + 模型多选（renderAIProviders / openAIProviderModal 等）
 * - ai/core.js       — init + config 加载 + UI 应用 + 事件绑定（initAITab / loadAIConfig / bindAIEvents）
 *
 * 依赖方向（无循环）：
 *   state ← tier ← model-edit ← provider ← core ← ai.js(本文件)
 *   state ← skill ← core
 *
 * 循环依赖打破：
 *   provider.js → model-edit.js（单向 import openAIModelEditModal）
 *   model-edit.js 通过回调调用 provider.js 的 renderAIProviders / getExpandedProviderIds / restoreExpandedProviderIds
 */

import {initAITab} from "./ai/core.js";
import {aiState} from "./ai/state.js";
import {getExpandedProviderIds, renderAIProviders, restoreExpandedProviderIds} from "./ai/provider.js";

// 注册跨模块回调（打破 provider.js ↔ model-edit.js 循环依赖）
aiState._renderAIProviders = renderAIProviders;
aiState._getExpandedProviderIds = getExpandedProviderIds;
aiState._restoreExpandedProviderIds = restoreExpandedProviderIds;

export {initAITab};
