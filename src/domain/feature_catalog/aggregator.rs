//! FeatureCatalog 聚合器（0.21.4）。
//!
//! 从多个数据源聚合功能目录项：
//! - `BuiltinEngine::ACTIONS` descriptor（13 无参 + 3 参数化）
//! - `ChordRegistry` binding（6 个 chord action）
//! - `CapabilityRegistry` entries（builtin + plugin capability）
//!
//! 聚合策略：
//! 1. 以 builtin descriptor 为基准，每个 descriptor 生成一个 FeatureCatalogItem。
//! 2. Chord binding 按 capability_id 关联到已有 item（补充 binding），或独立成项。
//! 3. CapabilityRegistry 中未被 descriptor/chord 覆盖的 capability 独立成项。
//! 4. 未知/已移除 id 保留为残留项，标记 `SourceUnavailable`。

use std::collections::{HashMap, HashSet};

use super::types::*;

use crate::domain::capability::{
    Capability, CapabilityPolicy, CapabilityRegistry, DangerClass, InvocationOrigin,
    RuntimeRequirement,
};
use crate::domain::chord::{ChordRegistry, ChordTarget};
use crate::domain::plugin::PluginEngine;
use crate::domain::search::{list_builtin_actions, list_builtin_context_bindings};

/// FeatureCatalog 聚合器——纯数据聚合，不持有状态。
///
/// 每次调用 `aggregate()` 从传入的各 registry / config 重建目录。
/// 调用方（command 层）负责传入最新的 config 快照。
pub struct FeatureCatalogAggregator;

impl FeatureCatalogAggregator {
    /// 聚合所有数据源，生成功能目录。
    ///
    /// # 参数
    /// - `disabled_builtin`: 用户禁用的 builtin action id 列表
    /// - `disabled_chord`: 用户禁用的 chord action id 列表
    /// - `disabled_context`: 用户禁用的 context binding key 列表
    /// - `language`: 当前 UI 语言（"zh" / "en" 等）
    /// - `cap_registry`: CapabilityRegistry（已注册的 capability）
    /// - `chord_registry`: ChordRegistry（已注册的 chord action）
    /// - `plugin_engine`: 可选的 PluginEngine（None = CLI/无插件环境）
    /// - `ai_allowlist`: 用户 AI 授权集合（`ai.capability_access` 真源）；
    ///   None = 无授权数据（CLI 等环境），AI 列退回 policy 默认投影
    /// - `mcp_exposed`: 用户 MCP 暴露集合（`exposed_capabilities` 真源）
    #[allow(clippy::too_many_arguments)]
    pub fn aggregate(
        disabled_builtin: &[String],
        disabled_chord: &[String],
        disabled_context: &[String],
        language: &str,
        cap_registry: &CapabilityRegistry,
        chord_registry: &ChordRegistry,
        plugin_engine: Option<&PluginEngine>,
        ai_allowlist: Option<&HashSet<String>>,
        mcp_exposed: &HashSet<String>,
    ) -> Vec<FeatureCatalogItem> {
        let use_en = language.starts_with("en");
        let disabled_context_set: HashSet<&str> =
            disabled_context.iter().map(|s| s.as_str()).collect();

        // ── Step 1: builtin descriptor → 目录项 ──────────────────────────
        // list_builtin_actions 接收 disabled_builtin_actions，内部读取 descriptor 双语 title
        let builtin_actions = list_builtin_actions(disabled_builtin, language);
        // list_builtin_context_bindings 接收 disabled_context_bindings，但当前 disabled_builtin
        // 是 disabled_builtin_actions（不同分片）。context binding 的 disabled 状态由
        // disabled_context_set 在下面单独判定。
        let context_bindings = list_builtin_context_bindings(disabled_context, language);

        // 用 feature_id → index 做查重，chord binding 和 capability 都靠这个关联
        let mut items: Vec<FeatureCatalogItem> = Vec::new();
        let mut feature_id_to_index: HashMap<String, usize> = HashMap::new();
        // capability_id → feature_id 的映射，chord binding 和独立 capability 据此关联
        let mut cap_id_to_feature_id: HashMap<String, String> = HashMap::new();

        for action in &builtin_actions {
            let cap_id = &action.id; // 0.21.3: descriptor id == capability_id
            let feature_id = format!("blink.{}", cap_id);
            let group = FeatureGroup::infer_from_descriptor_id(cap_id);

            // 收集该 descriptor 的 binding
            let mut bindings = Vec::new();

            // Context 门禁动作（参数化，声明了 context triggers）：关键词命中也必须
            // Context 命中才召回（builtin_engine::search 2a 门禁）——单独展示
            // "关键词：…"会误导用户以为输入关键词即可唤起。改为：
            // - 不生成关键词提示；
            // - 以 context 触发条件（裸 trigger key，前端按 context.trigger.* 翻译）
            //   作为唯一本地入口标签；
            // - 开关仍写 action 级 disabled_builtin_actions（SearchKeyword store），
            //   等价关闭该动作的全部召回路径；binding 粒度的 context 禁用由
            //   "Ghost 触发规则"面板管理，不在目录内重复表达。
            if action.context_gated {
                let trigger_key = context_bindings
                    .iter()
                    .find(|ctx| {
                        ctx["target_id"]
                            .as_str()
                            .unwrap_or_default()
                            .strip_prefix("builtin:")
                            .map(|id| id == cap_id.as_str())
                            .unwrap_or(false)
                    })
                    .map(|ctx| {
                        ctx["trigger_label"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string()
                    })
                    .unwrap_or_else(|| cap_id.clone());
                bindings.push(BindingSummary {
                    binding_id: cap_id.clone(),
                    kind: BindingKind::SearchKeyword,
                    enabled: action.enabled,
                    trigger_label: trigger_key,
                });
            } else {
                // SearchKeyword binding（无门禁动作：关键词可独立召回）
                bindings.push(BindingSummary {
                    binding_id: cap_id.clone(),
                    kind: BindingKind::SearchKeyword,
                    enabled: action.enabled,
                    trigger_label: if use_en {
                        format!("Keywords: {}", action.keywords.join(", "))
                    } else {
                        format!("关键词：{}", action.keywords.join("、"))
                    },
                });

                // context binding 的 target_id 格式为 "builtin:{action_id}"
                // 需要提取 action_id 来匹配 descriptor
                for ctx in &context_bindings {
                    let ctx_target_id = ctx["target_id"].as_str().unwrap_or_default();
                    // target_id 格式 "builtin:open_url" → 提取 "open_url"
                    let ctx_action_id = ctx_target_id
                        .strip_prefix("builtin:")
                        .unwrap_or(ctx_target_id);
                    if ctx_action_id == *cap_id {
                        let ctx_key = ctx["key"].as_str().unwrap_or_default().to_string();
                        let enabled = !disabled_context_set.contains(ctx_key.as_str());
                        bindings.push(BindingSummary {
                            binding_id: ctx_key,
                            kind: BindingKind::ContextBinding,
                            enabled,
                            trigger_label: ctx["trigger_label"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        });
                    }
                }
            }

            // 本地可用性
            let local_availability = if !action.enabled {
                LocalAvailability::Disabled
            } else {
                LocalAvailability::Available
            };

            // Capability 投影
            let cap_projection = cap_registry.get(cap_id).map(|cap| {
                build_capability_projection(&cap, FeatureSource::Builtin, ai_allowlist, mcp_exposed)
            });

            let unavailable_reason = if local_availability != LocalAvailability::Available {
                if !action.enabled {
                    Some(if use_en {
                        "Disabled by user".into()
                    } else {
                        "用户已禁用".into()
                    })
                } else {
                    None
                }
            } else {
                None
            };

            cap_id_to_feature_id.insert(cap_id.clone(), feature_id.clone());
            feature_id_to_index.insert(feature_id.clone(), items.len());

            items.push(FeatureCatalogItem {
                feature_id,
                title: action.title.clone(),
                description: action.subtitle.clone(),
                group,
                source: FeatureSource::Builtin,
                capability_id: Some(cap_id.clone()),
                bindings,
                local_availability,
                capability_projection: cap_projection,
                unavailable_reason,
            });
        }

        // ── Step 2: chord binding → 补充到已有 item 或独立成项 ──────────
        let chord_list = chord_registry.list_all(disabled_chord, &Default::default(), language);
        for chord in &chord_list {
            let chord_id = chord["id"].as_str().unwrap_or_default().to_string();
            let label = chord["label"].as_str().unwrap_or_default().to_string();
            let key = chord["key"].as_str().unwrap_or_default().to_string();
            let enabled = chord["enabled"].as_bool().unwrap_or(true);

            // 查找 chord 的 target capability_id
            let target_cap_id: Option<String> = chord_registry
                .actions_iter()
                .find(|a| a.id() == chord_id)
                .and_then(|a| match a.target() {
                    ChordTarget::Capability { capability_id, .. } => {
                        Some(capability_id.to_string())
                    }
                    ChordTarget::VoiceInteraction => None,
                });

            // 空格键显示为 "Space" 文本（voice_input 的 chord 键是 ' '，
            // 裸拼会产出 "Alt+ " 尾随空格；对齐 hotkey recorder 的 display 规则）
            let key_display = if key == " " {
                "Space".to_string()
            } else {
                key.to_uppercase()
            };
            let trigger_label = if key.is_empty() {
                label.clone()
            } else if use_en {
                format!("Alt+{} (hold)", key_display)
            } else {
                format!("Alt+{}", key_display)
            };

            let binding_summary = BindingSummary {
                binding_id: format!("chord.{}", chord_id),
                kind: BindingKind::ChordKey,
                enabled,
                trigger_label,
            };

            if let Some(ref cap_id) = target_cap_id {
                // 有关联的 capability——尝试补充到已有 item
                if let Some(feat_id) = cap_id_to_feature_id.get(cap_id)
                    && let Some(&idx) = feature_id_to_index.get(feat_id)
                {
                    items[idx].bindings.push(binding_summary);
                    // 如果 chord 被 disable 但 descriptor 没被 disable，
                    // 本地可用性应反映 chord 的 disable 状态
                    if !enabled && items[idx].local_availability == LocalAvailability::Available {
                        // chord disabled 不等于整个功能不可用——
                        // 只是 chord binding 不可用。保持 Available，binding.enabled=false。
                        // 但如果该功能只有 chord binding 且 chord 被禁用，则应标记 Disabled。
                        // 当前所有 chord 对应的 capability 也有 descriptor，所以保持 Available。
                    }
                    continue;
                }

                // capability 存在但没有对应的 descriptor item——独立成项
                let feature_id = format!("chord.{}", chord_id);
                let group = FeatureGroup::infer_from_capability_id(cap_id);
                let cap_projection = cap_registry.get(cap_id).map(|cap| {
                    build_capability_projection(
                        &cap,
                        FeatureSource::Chord,
                        ai_allowlist,
                        mcp_exposed,
                    )
                });

                cap_id_to_feature_id.insert(cap_id.clone(), feature_id.clone());
                feature_id_to_index.insert(feature_id.clone(), items.len());

                items.push(FeatureCatalogItem {
                    feature_id,
                    title: label.clone(),
                    description: String::new(),
                    group,
                    source: FeatureSource::Chord,
                    capability_id: Some(cap_id.clone()),
                    bindings: vec![binding_summary],
                    local_availability: if !enabled {
                        LocalAvailability::Disabled
                    } else {
                        LocalAvailability::Available
                    },
                    capability_projection: cap_projection,
                    unavailable_reason: if !enabled {
                        Some(if use_en {
                            "Chord disabled".into()
                        } else {
                            "Chord 已禁用".into()
                        })
                    } else {
                        None
                    },
                });
            } else {
                // VoiceInteraction —— Interaction-only，无 capability
                let feature_id = format!("chord.{}", chord_id);
                feature_id_to_index.insert(feature_id.clone(), items.len());

                items.push(FeatureCatalogItem {
                    feature_id,
                    title: label,
                    description: if use_en {
                        "Voice interaction (hold to talk)".into()
                    } else {
                        "语音交互（按住说话）".into()
                    },
                    group: FeatureGroup::BlinkManagement,
                    source: FeatureSource::Chord,
                    capability_id: None,
                    bindings: vec![binding_summary],
                    local_availability: if !enabled {
                        LocalAvailability::Disabled
                    } else {
                        LocalAvailability::Available
                    },
                    capability_projection: None,
                    unavailable_reason: if !enabled {
                        Some(if use_en {
                            "Chord disabled".into()
                        } else {
                            "Chord 已禁用".into()
                        })
                    } else {
                        None
                    },
                });
            }
        }

        // ── Step 3: CapabilityRegistry 中未被覆盖的 capability 独立成项 ─
        let covered_cap_ids: HashSet<String> = cap_id_to_feature_id.keys().cloned().collect();

        // 收集所有插件 capability id（从 PluginEngine 的 manifest tools 派生）
        let plugin_cap_ids: HashSet<String> = if let Some(pe) = plugin_engine {
            pe.list_manifests()
                .iter()
                .flat_map(|manifest| {
                    manifest.tools.iter().map(move |tool| {
                        crate::domain::plugin::plugin_tool_id(&manifest.id, &tool.name)
                    })
                })
                .collect()
        } else {
            HashSet::new()
        };

        for (cap_id, cap_arc) in cap_registry.entries() {
            if covered_cap_ids.contains(&cap_id) {
                continue;
            }

            // 判断是插件还是 builtin：检查是否在插件 tool id 集合中
            let is_plugin = plugin_cap_ids.contains(&cap_id);
            let source = if is_plugin {
                FeatureSource::Plugin
            } else {
                FeatureSource::BuiltinCapability
            };

            let group = if is_plugin {
                FeatureGroup::OtherPlugin
            } else {
                FeatureGroup::infer_from_capability_id(&cap_id)
            };

            let feature_id = if is_plugin {
                format!("plugin.{}", cap_id)
            } else {
                format!("blink.{}", cap_id)
            };

            let schema = cap_arc.schema();
            // 目录展示拆分：title = 用户可读短名，description = schema 长句。
            // schema.description 面向 AI 工具调用（含参数/返回说明），此前直接当
            // title 会产出"只有超长标题没有描述"的目录行（0.21.10 拆分）。
            let title = if is_plugin {
                // 尝试从 plugin manifest 获取名称，缺失回退人类化 id
                plugin_engine
                    .and_then(|pe| find_plugin_name_for_cap(pe, &cap_id, language))
                    .unwrap_or_else(|| humanize_capability_id(&cap_id))
            } else {
                capability_short_title(&cap_id, use_en)
            };

            let cap_projection =
                build_capability_projection(&cap_arc, source, ai_allowlist, mcp_exposed);

            // 插件可用性
            let (local_availability, unavailable_reason) = if is_plugin {
                if let Some(pe) = plugin_engine {
                    // 从 plugin_cap_ids 反查 plugin_id
                    let plugin_id = find_plugin_id_for_cap(pe, &cap_id);
                    if let Some(pid) = plugin_id {
                        if !pe.is_enabled(&pid) {
                            (
                                LocalAvailability::Disabled,
                                Some(if use_en {
                                    "Plugin disabled".into()
                                } else {
                                    "插件已禁用".into()
                                }),
                            )
                        } else {
                            (LocalAvailability::Available, None)
                        }
                    } else {
                        (LocalAvailability::Available, None)
                    }
                } else {
                    (
                        LocalAvailability::SourceUnavailable,
                        Some(if use_en {
                            "Plugin engine not available".into()
                        } else {
                            "插件引擎不可用".into()
                        }),
                    )
                }
            } else {
                (LocalAvailability::Available, None)
            };

            items.push(FeatureCatalogItem {
                feature_id,
                title,
                description: schema.description.clone(),
                group,
                source,
                capability_id: Some(cap_id.clone()),
                bindings: Vec::new(),
                local_availability,
                capability_projection: Some(cap_projection),
                unavailable_reason,
            });
        }

        // ── Step 4: 排序——组升序 → 组内 Chord > 插件 > 普通 → title 升序 ──
        // （Chord/插件是用户显式配置的入口，排在组内前部更易定位）
        items.sort_by(|a, b| {
            a.group
                .cmp(&b.group)
                .then(source_rank(&a.source).cmp(&source_rank(&b.source)))
                .then_with(|| a.title.cmp(&b.title))
        });

        items
    }
}

/// 组内排序的来源优先级：Chord=0 > Plugin=1 > 普通（Builtin/BuiltinCapability）=2。
fn source_rank(source: &FeatureSource) -> u8 {
    match source {
        FeatureSource::Chord => 0,
        FeatureSource::Plugin => 1,
        _ => 2,
    }
}

// ── 辅助函数 ─────────────────────────────────────────────────────────────────

/// 无 descriptor/chord 覆盖的内置 capability 目录短标题（双语）。
///
/// 这些能力只有面向 AI 的 `schema.description`（含参数/返回的长句），目录展示
/// 需要用户可读短名——与 descriptor 的双语 title 同一风格。未收录的新 id 由
/// `humanize_capability_id` 兜底，新增能力时在此补一行即可。
const CAPABILITY_TITLES: &[(&str, &str, &str)] = &[
    ("analyze_image_palette", "图片取色", "Analyze Image Palette"),
    ("get_settings", "读取设置", "Read Settings"),
    (
        "list_clipboard_images",
        "剪贴板图片列表",
        "List Clipboard Images",
    ),
    ("list_sticky", "便签列表", "List Sticky Notes"),
    ("list_windows", "窗口列表", "List Windows"),
    ("ocr_image", "图片识别文字", "OCR Image"),
    ("pin_image", "贴图", "Pin Image"),
    ("read_clipboard", "读取剪贴板", "Read Clipboard"),
    (
        "read_clipboard_history_image",
        "读取剪贴板历史图片",
        "Read Clipboard History Image",
    ),
    ("read_sticky", "读取便签", "Read Sticky Note"),
    ("read_text_file", "读取文本文件", "Read Text File"),
    ("screenshot", "截图", "Screenshot"),
    ("search_apps", "搜索应用", "Search Apps"),
    (
        "search_clipboard_history",
        "搜索剪贴板历史",
        "Search Clipboard History",
    ),
    ("search_files", "搜索文件", "Search Files"),
    ("set_sticky_geometry", "设置便签位置", "Set Sticky Geometry"),
    (
        "set_sticky_visibility",
        "设置便签可见性",
        "Set Sticky Visibility",
    ),
    ("trash_sticky", "回收便签", "Trash Sticky Note"),
    ("update_setting", "更新设置", "Update Setting"),
    ("update_sticky", "更新便签", "Update Sticky Note"),
    ("write_clipboard", "写入剪贴板", "Write Clipboard"),
];

/// capability-only 目录项的用户可读短标题；未收录 id 回退人类化 id。
fn capability_short_title(cap_id: &str, use_en: bool) -> String {
    if let Some((_, zh, en)) = CAPABILITY_TITLES.iter().find(|(id, _, _)| *id == cap_id) {
        return if use_en {
            (*en).to_string()
        } else {
            (*zh).to_string()
        };
    }
    humanize_capability_id(cap_id)
}

/// snake_case id → 空格分隔 + 首字母大写（短名表未收录时的兜底标题）。
fn humanize_capability_id(cap_id: &str) -> String {
    let replaced = cap_id.replace('_', " ");
    let mut chars = replaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// 从 PluginEngine 查找插件 capability 对应的 plugin_id。
///
/// 遍历所有 manifest 的 tools，用 `plugin_tool_id` 构造 id 并比对。
fn find_plugin_id_for_cap(pe: &PluginEngine, cap_id: &str) -> Option<String> {
    for manifest in pe.list_manifests() {
        for tool in &manifest.tools {
            let id = crate::domain::plugin::plugin_tool_id(&manifest.id, &tool.name);
            if id == cap_id {
                return Some(manifest.id.clone());
            }
        }
    }
    None
}

/// 从 PluginEngine 查找插件 capability 对应的插件显示名；解析不到返回 None
/// （调用方以人类化 id 兜底，0.21.10 起不再拿 schema 长句当标题）。
fn find_plugin_name_for_cap(pe: &PluginEngine, cap_id: &str, language: &str) -> Option<String> {
    let plugin_id = find_plugin_id_for_cap(pe, cap_id)?;
    let manifest = pe.get_manifest(&plugin_id)?;
    Some(manifest.name.resolve(language))
}

/// 从 Capability 构建 CatalogCapabilityProjection。
///
/// `ai_allowlist` / `mcp_exposed` 是用户授权的真源快照（§3.4/§3.5）；
/// 出口状态反映**实际授权**，不是 policy 默认值。
fn build_capability_projection(
    cap: &std::sync::Arc<dyn Capability>,
    source: FeatureSource,
    ai_allowlist: Option<&HashSet<String>>,
    mcp_exposed: &HashSet<String>,
) -> CatalogCapabilityProjection {
    let schema = cap.schema();
    let policy = cap.policy();

    let danger = match policy.danger {
        DangerClass::Safe => "safe",
        DangerClass::Dangerous => "dangerous",
    };

    let ai_status = project_ai_exit_status(&policy, cap.id(), ai_allowlist);
    let mcp_status = project_mcp_exit_status(&policy, cap.id(), mcp_exposed);

    let runtime_requirement = format_runtime_requirement(policy.runtime_requirement);

    CatalogCapabilityProjection {
        capability_id: cap.id().to_string(),
        source,
        danger: danger.to_string(),
        sensitive: policy.sensitive,
        requires_confirmation: policy.requires_confirmation(),
        ai_status,
        mcp_status,
        runtime_requirement,
        description: schema.description,
    }
}

/// 投影 AI 出口状态。
///
/// - origin 不允许 AI → 代码级禁止；
/// - 有用户 allowlist 时以 allowlist 为准（Enabled/Disabled）；
/// - 无 allowlist 数据（None，CLI 等环境）退回 `ai_default` 投影。
fn project_ai_exit_status(
    policy: &CapabilityPolicy,
    cap_id: &str,
    ai_allowlist: Option<&HashSet<String>>,
) -> CatalogExitStatus {
    if !policy.allowed_origins.contains(InvocationOrigin::LocalAi) {
        return CatalogExitStatus::CodeForbidden;
    }
    if let Some(allowlist) = ai_allowlist {
        if allowlist.contains(cap_id) {
            CatalogExitStatus::Enabled
        } else {
            CatalogExitStatus::Disabled
        }
    } else {
        match policy.ai_default {
            crate::domain::capability::AiDefault::On => CatalogExitStatus::Enabled,
            crate::domain::capability::AiDefault::Off => CatalogExitStatus::Disabled,
        }
    }
}

/// 投影 MCP 出口状态。
///
/// - origin 不允许 MCP、Dangerous、`mcp_default == Forbidden` → 代码级禁止
///   （§3.5：MCP 首版禁止 Dangerous，配置不能绕过）；
/// - 其余以用户 `exposed_capabilities` 为准。
fn project_mcp_exit_status(
    policy: &CapabilityPolicy,
    cap_id: &str,
    mcp_exposed: &HashSet<String>,
) -> CatalogExitStatus {
    if !policy.allowed_origins.contains(InvocationOrigin::Mcp)
        || policy.danger == DangerClass::Dangerous
        || policy.mcp_default == crate::domain::capability::McpDefault::Forbidden
    {
        return CatalogExitStatus::CodeForbidden;
    }
    if mcp_exposed.contains(cap_id) {
        CatalogExitStatus::Enabled
    } else {
        CatalogExitStatus::Disabled
    }
}

/// 格式化运行时要求为人类可读字符串。
///
/// `RuntimeRequirement` 已实现 `Display`，直接复用。
fn format_runtime_requirement(req: RuntimeRequirement) -> String {
    req.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability::{
        AiDefault, CapabilityError, CapabilityRegistry, CapabilityResult, CapabilitySchema,
        ConfirmationPolicy, DangerClass, McpDefault, OriginSet, RuntimeRequirement,
    };
    use crate::domain::chord::{ChordAction, ChordRegistry, ChordSurface, ChordTarget};
    use crate::domain::plugin::LocalizableText;
    use std::sync::Arc;

    // ── 辅助：构造 mock Capability ──────────────────────────────────────────

    /// 测试用 mock Capability——可自定义 id、schema、policy。
    struct MockCap {
        id_val: &'static str,
        schema_val: CapabilitySchema,
        policy_val: CapabilityPolicy,
    }

    #[async_trait::async_trait]
    impl crate::domain::capability::Capability for MockCap {
        fn id(&self) -> &str {
            self.id_val
        }
        fn schema(&self) -> CapabilitySchema {
            self.schema_val.clone()
        }
        fn policy(&self) -> CapabilityPolicy {
            self.policy_val.clone()
        }
        async fn invoke(
            &self,
            _args: serde_json::Value,
            _ctx: &crate::domain::capability::InvokeContext<'_>,
        ) -> Result<CapabilityResult, CapabilityError> {
            Ok(CapabilityResult::Done {
                summary: "mock".into(),
            })
        }
    }

    /// 便利构造：Safe + 无运行时要求 + 全出口允许。
    fn make_mock_cap(id: &'static str) -> Arc<dyn Capability> {
        Arc::new(MockCap {
            id_val: id,
            schema_val: CapabilitySchema::empty(id, "mock capability"),
            policy_val: CapabilityPolicy::default(),
        })
    }

    /// 便利构造：自定义 policy。
    fn make_mock_cap_with_policy(
        id: &'static str,
        policy: CapabilityPolicy,
    ) -> Arc<dyn Capability> {
        Arc::new(MockCap {
            id_val: id,
            schema_val: CapabilitySchema::empty(id, "mock capability"),
            policy_val: policy,
        })
    }

    // ── 辅助：构造 mock ChordAction ─────────────────────────────────────────

    /// 测试用 mock ChordAction。
    struct MockChordAction {
        id_val: &'static str,
        key_val: char,
        label_val: LocalizableText,
        target_val: ChordTarget,
    }

    impl ChordAction for MockChordAction {
        fn id(&self) -> &str {
            self.id_val
        }
        fn default_key(&self) -> char {
            self.key_val
        }
        fn label(&self) -> &LocalizableText {
            &self.label_val
        }
        fn surface(&self) -> ChordSurface {
            ChordSurface::Default
        }
        fn target(&self) -> ChordTarget {
            self.target_val.clone()
        }
    }

    /// 便利构造：ChordTarget::Capability 的 chord action。
    fn make_capability_chord(
        id: &'static str,
        key: char,
        cap_id: &'static str,
        label: &str,
    ) -> Arc<dyn ChordAction> {
        Arc::new(MockChordAction {
            id_val: id,
            key_val: key,
            label_val: LocalizableText::Plain(label.to_string()),
            target_val: ChordTarget::Capability {
                capability_id: cap_id,
                input_param: None,
                extra_args: Vec::new(),
                hide_main_before: false,
            },
        })
    }

    /// 便利构造：ChordTarget::VoiceInteraction 的 chord action。
    fn make_voice_chord(id: &'static str, key: char, label: &str) -> Arc<dyn ChordAction> {
        Arc::new(MockChordAction {
            id_val: id,
            key_val: key,
            label_val: LocalizableText::Plain(label.to_string()),
            target_val: ChordTarget::VoiceInteraction,
        })
    }

    // ── 辅助：构造空 PluginEngine（测试环境无插件） ──────────────────────────
    // PluginEngine 需要 SqlitePool，在单元测试中不构造。
    // aggregate 的 plugin_engine 参数传 None 即可测试非插件路径。

    // ── 1. FeatureGroup 推断 ──────────────────────────────────────────────────

    #[test]
    fn infer_group_for_known_ids() {
        assert_eq!(
            FeatureGroup::infer_from_capability_id("open_url"),
            FeatureGroup::AppsFilesLinks
        );
        assert_eq!(
            FeatureGroup::infer_from_capability_id("read_clipboard"),
            FeatureGroup::ClipboardText
        );
        assert_eq!(
            FeatureGroup::infer_from_capability_id("screenshot"),
            FeatureGroup::ImageColor
        );
        assert_eq!(
            FeatureGroup::infer_from_capability_id("create_sticky"),
            FeatureGroup::StickyContent
        );
        assert_eq!(
            FeatureGroup::infer_from_capability_id("lock"),
            FeatureGroup::WindowSystem
        );
        assert_eq!(
            FeatureGroup::infer_from_capability_id("open_settings"),
            FeatureGroup::BlinkManagement
        );
    }

    #[test]
    fn infer_group_for_unknown_id_is_other() {
        assert_eq!(
            FeatureGroup::infer_from_capability_id("some_random_cap"),
            FeatureGroup::OtherPlugin
        );
    }

    #[test]
    fn infer_group_from_descriptor_id_matches_capability_id() {
        // 0.21.3: descriptor id == capability_id，infer 结果应一致
        assert_eq!(
            FeatureGroup::infer_from_descriptor_id("open_url"),
            FeatureGroup::infer_from_capability_id("open_url")
        );
        assert_eq!(
            FeatureGroup::infer_from_descriptor_id("screenshot"),
            FeatureGroup::infer_from_capability_id("screenshot")
        );
    }

    // ── 2. plugin_tool_id 格式 ────────────────────────────────────────────────

    #[test]
    fn plugin_tool_id_format() {
        assert_eq!(
            crate::domain::plugin::plugin_tool_id("translate", "translate"),
            "translate_translate"
        );
        assert_eq!(
            crate::domain::plugin::plugin_tool_id("weather", "get_weather"),
            "weather_get_weather"
        );
        assert_eq!(
            crate::domain::plugin::plugin_tool_id("echo", "echo"),
            "echo_echo"
        );
    }

    // ── 3. RuntimeRequirement 格式化 ──────────────────────────────────────────

    #[test]
    fn format_runtime_none() {
        assert_eq!(format_runtime_requirement(RuntimeRequirement::NONE), "none");
    }

    #[test]
    fn format_runtime_combined() {
        let req = RuntimeRequirement::DESKTOP_SESSION | RuntimeRequirement::MAIN_PROCESS;
        let formatted = format_runtime_requirement(req);
        assert!(formatted.contains("main_process"));
        assert!(formatted.contains("desktop_session"));
    }

    #[test]
    fn format_runtime_gui_surface() {
        let formatted = format_runtime_requirement(RuntimeRequirement::GUI_SURFACE);
        // Display 不调用 normalize()，GUI_SURFACE 只输出自身
        assert!(formatted.contains("gui_surface"));
    }

    // ── 4. AI/MCP 出口状态投影 ─────────────────────────────────────────────────

    #[test]
    fn project_ai_status_forbidden() {
        let policy = CapabilityPolicy {
            allowed_origins: OriginSet::LOCAL_SURFACE,
            ..Default::default()
        };
        assert_eq!(
            project_ai_exit_status(&policy, "cap_x", Some(&HashSet::new())),
            CatalogExitStatus::CodeForbidden
        );
    }

    #[test]
    fn project_ai_status_follows_allowlist_when_present() {
        let policy = CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            ai_default: AiDefault::On,
            ..Default::default()
        };
        let allowlist: HashSet<String> = ["cap_in".to_string()].into_iter().collect();
        // allowlist 包含 → Enabled（ai_default 不参与判定）
        assert_eq!(
            project_ai_exit_status(&policy, "cap_in", Some(&allowlist)),
            CatalogExitStatus::Enabled
        );
        // allowlist 不包含 → Disabled——即使 ai_default == On
        assert_eq!(
            project_ai_exit_status(&policy, "cap_out", Some(&allowlist)),
            CatalogExitStatus::Disabled
        );
    }

    #[test]
    fn project_ai_status_falls_back_to_default_without_allowlist() {
        // None（CLI 等无授权数据环境）→ 退回 ai_default 投影
        let policy_on = CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            ai_default: AiDefault::On,
            ..Default::default()
        };
        assert_eq!(
            project_ai_exit_status(&policy_on, "cap_x", None),
            CatalogExitStatus::Enabled
        );
        let policy_off = CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            ai_default: AiDefault::Off,
            ..Default::default()
        };
        assert_eq!(
            project_ai_exit_status(&policy_off, "cap_x", None),
            CatalogExitStatus::Disabled
        );
    }

    #[test]
    fn project_mcp_status_forbidden() {
        let policy = CapabilityPolicy {
            allowed_origins: OriginSet::LOCAL_SURFACE,
            ..Default::default()
        };
        assert_eq!(
            project_mcp_exit_status(&policy, "cap_x", &HashSet::new()),
            CatalogExitStatus::CodeForbidden
        );
    }

    #[test]
    fn project_mcp_status_code_forbidden_via_mcp_default() {
        let policy = CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            mcp_default: McpDefault::Forbidden,
            ..Default::default()
        };
        // exposed 即使包含该 id 也无法绕过代码级禁止
        let exposed: HashSet<String> = ["cap_x".to_string()].into_iter().collect();
        assert_eq!(
            project_mcp_exit_status(&policy, "cap_x", &exposed),
            CatalogExitStatus::CodeForbidden
        );
    }

    #[test]
    fn project_mcp_status_dangerous_always_code_forbidden() {
        // §3.5：MCP 首版禁止 Dangerous，即使 origin 允许
        let policy = CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            danger: DangerClass::Dangerous,
            ..Default::default()
        };
        let exposed: HashSet<String> = ["cap_x".to_string()].into_iter().collect();
        assert_eq!(
            project_mcp_exit_status(&policy, "cap_x", &exposed),
            CatalogExitStatus::CodeForbidden
        );
    }

    #[test]
    fn project_mcp_status_follows_exposed_set() {
        let policy = CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            mcp_default: McpDefault::DefaultOff,
            ..Default::default()
        };
        let exposed: HashSet<String> = ["cap_exposed".to_string()].into_iter().collect();
        assert_eq!(
            project_mcp_exit_status(&policy, "cap_exposed", &exposed),
            CatalogExitStatus::Enabled
        );
        assert_eq!(
            project_mcp_exit_status(&policy, "cap_hidden", &exposed),
            CatalogExitStatus::Disabled
        );
    }

    // ── 5. build_capability_projection ─────────────────────────────────────────

    #[test]
    fn build_projection_for_safe_cap() {
        let cap = make_mock_cap("test_safe_cap");
        let proj = build_capability_projection(&cap, FeatureSource::Builtin, None, &HashSet::new());

        assert_eq!(proj.capability_id, "test_safe_cap");
        assert_eq!(proj.source, FeatureSource::Builtin);
        assert_eq!(proj.danger, "safe");
        assert!(!proj.sensitive);
        assert!(!proj.requires_confirmation);
        assert_eq!(proj.runtime_requirement, "none");
    }

    #[test]
    fn build_projection_for_dangerous_cap() {
        let cap = make_mock_cap_with_policy(
            "test_dangerous_cap",
            CapabilityPolicy {
                danger: DangerClass::Dangerous,
                confirmation: ConfirmationPolicy::dangerous(true),
                ..Default::default()
            },
        );
        let proj = build_capability_projection(&cap, FeatureSource::Builtin, None, &HashSet::new());

        assert_eq!(proj.danger, "dangerous");
        assert!(proj.requires_confirmation);
    }

    #[test]
    fn build_projection_for_sensitive_cap() {
        let cap = make_mock_cap_with_policy(
            "test_sensitive_cap",
            CapabilityPolicy {
                sensitive: true,
                confirmation: ConfirmationPolicy::sensitive(),
                ..Default::default()
            },
        );
        let proj = build_capability_projection(&cap, FeatureSource::Builtin, None, &HashSet::new());

        assert!(proj.sensitive);
        assert!(proj.requires_confirmation);
        // sensitive 不改变 danger
        assert_eq!(proj.danger, "safe");
    }

    #[test]
    fn build_projection_preserves_source() {
        for source in [
            FeatureSource::Builtin,
            FeatureSource::Chord,
            FeatureSource::Plugin,
            FeatureSource::BuiltinCapability,
        ] {
            let cap = make_mock_cap("test_source_cap");
            let proj = build_capability_projection(&cap, source, None, &HashSet::new());
            assert_eq!(proj.source, source);
        }
    }

    // ── 6. 聚合器集成测试 ─────────────────────────────────────────────────────

    #[test]
    fn aggregate_builtin_descriptors() {
        // 空 registry + 空 chord → 只返回 builtin descriptor 项
        let cap_reg = CapabilityRegistry::default();
        let chord_reg = ChordRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        // 至少有 builtin action（list_builtin_actions 返回的条目数）
        assert!(!items.is_empty(), "builtin descriptor 目录不应为空");

        // Builtin source 的项应有 SearchKeyword binding
        for item in items.iter().filter(|i| i.source == FeatureSource::Builtin) {
            assert!(
                item.bindings
                    .iter()
                    .any(|b| b.kind == BindingKind::SearchKeyword),
                "builtin feature {} 应至少有 SearchKeyword binding",
                item.feature_id
            );
            assert!(
                item.feature_id.starts_with("blink."),
                "builtin feature_id 应以 blink. 开头: {}",
                item.feature_id
            );
        }

        // BuiltinCapability source 的项（从 CapabilityRegistry inventory 补充的）
        // 可能有空 bindings（无 descriptor/chord 关联）
        for item in items
            .iter()
            .filter(|i| i.source == FeatureSource::BuiltinCapability)
        {
            assert!(
                item.feature_id.starts_with("blink."),
                "builtin_capability feature_id 应以 blink. 开头: {}",
                item.feature_id
            );
        }
    }

    #[test]
    fn aggregate_with_disabled_builtin() {
        let cap_reg = CapabilityRegistry::default();
        let chord_reg = ChordRegistry::default();

        // 禁用 open_settings
        let items = FeatureCatalogAggregator::aggregate(
            &["open_settings".to_string()],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        let settings_item = items
            .iter()
            .find(|i| i.feature_id == "blink.open_settings")
            .expect("应找到 open_settings 目录项");

        assert_eq!(
            settings_item.local_availability,
            LocalAvailability::Disabled
        );
        assert!(settings_item.unavailable_reason.is_some());
        // SearchKeyword binding 的 enabled 应为 false
        let sk_binding = settings_item
            .bindings
            .iter()
            .find(|b| b.kind == BindingKind::SearchKeyword)
            .expect("应有 SearchKeyword binding");
        assert!(!sk_binding.enabled);
    }

    #[test]
    fn aggregate_chord_supplements_existing_item() {
        // 构造 chord action 指向 screenshot capability
        // screenshot 是 builtin descriptor 之一，chord 应补充到已有 item
        let mut chord_reg = ChordRegistry::new();
        chord_reg.register(make_capability_chord(
            "screenshot_test",
            'a',
            "screenshot",
            "截图",
        ));

        let cap_reg = CapabilityRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        // screenshot 的目录项应有 ChordKey binding
        let screenshot_item = items
            .iter()
            .find(|i| i.capability_id.as_deref() == Some("screenshot"))
            .expect("应找到 screenshot 目录项");

        let chord_binding = screenshot_item
            .bindings
            .iter()
            .find(|b| b.kind == BindingKind::ChordKey)
            .expect("screenshot 应有 ChordKey binding");

        assert!(chord_binding.enabled);
        assert!(chord_binding.binding_id.starts_with("chord."));
    }

    #[test]
    fn aggregate_chord_voice_interaction_standalone() {
        // VoiceInteraction chord 无 capability，应独立成项
        let mut chord_reg = ChordRegistry::new();
        chord_reg.register(make_voice_chord("voice_test", 'v', "语音输入"));

        let cap_reg = CapabilityRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        let voice_item = items
            .iter()
            .find(|i| i.feature_id == "chord.voice_test")
            .expect("应找到 voice_test 独立目录项");

        assert_eq!(voice_item.source, FeatureSource::Chord);
        assert!(voice_item.capability_id.is_none());
        assert_eq!(voice_item.group, FeatureGroup::BlinkManagement);
        assert_eq!(voice_item.local_availability, LocalAvailability::Available);
    }

    #[test]
    fn aggregate_chord_disabled() {
        let mut chord_reg = ChordRegistry::new();
        chord_reg.register(make_voice_chord("disabled_voice", 'v', "语音"));

        let cap_reg = CapabilityRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &["disabled_voice".to_string()],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        let voice_item = items
            .iter()
            .find(|i| i.feature_id == "chord.disabled_voice")
            .expect("应找到 disabled_voice 目录项");

        assert_eq!(voice_item.local_availability, LocalAvailability::Disabled);
        assert!(voice_item.unavailable_reason.is_some());

        let chord_binding = voice_item
            .bindings
            .iter()
            .find(|b| b.kind == BindingKind::ChordKey)
            .expect("应有 ChordKey binding");
        assert!(!chord_binding.enabled);
    }

    #[test]
    fn aggregate_uncovered_capability_standalone() {
        // 注册一个不在 descriptor/chord 中的 capability
        let cap_reg = CapabilityRegistry::default();
        cap_reg.register(make_mock_cap("custom_cap_123")).unwrap();

        let chord_reg = ChordRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None, // 无 PluginEngine → 归为 BuiltinCapability
            None,
            &HashSet::new(),
        );

        let custom_item = items
            .iter()
            .find(|i| i.capability_id.as_deref() == Some("custom_cap_123"))
            .expect("应找到 custom_cap_123 目录项");

        assert_eq!(custom_item.source, FeatureSource::BuiltinCapability);
        assert_eq!(custom_item.feature_id, "blink.custom_cap_123");
        assert_eq!(custom_item.local_availability, LocalAvailability::Available);
        assert!(custom_item.capability_projection.is_some());
    }

    #[test]
    fn capability_only_item_splits_title_and_description() {
        // 0.21.10：无 descriptor/chord 覆盖的 capability 目录项，
        // title = 短名表用户可读标题，schema 长句进 description——
        // 不再出现"超长标题 + 空描述"的行；未收录 id 回退人类化。
        // search_files 经 inventory 自动注册（真实 capability），无需手动注册
        let cap_reg = CapabilityRegistry::default();
        cap_reg.register(make_mock_cap("custom_cap_123")).unwrap();

        let chord_reg = ChordRegistry::default();

        let zh_items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );
        let search = zh_items
            .iter()
            .find(|i| i.capability_id.as_deref() == Some("search_files"))
            .expect("应找到 search_files 目录项");
        assert_eq!(search.title, "搜索文件");
        // schema 长句（真实描述）完整落到 description，不再被截进 title
        assert_eq!(
            search.description,
            "按关键词搜索本地文件（Everything / 本地目录），返回匹配文件列表。"
        );

        let custom = zh_items
            .iter()
            .find(|i| i.capability_id.as_deref() == Some("custom_cap_123"))
            .expect("应找到 custom_cap_123 目录项");
        assert_eq!(custom.title, "Custom cap 123");
        assert_eq!(custom.description, "mock capability");

        // 英文语言走短名表 en 列
        let en_items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "en",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );
        let search_en = en_items
            .iter()
            .find(|i| i.capability_id.as_deref() == Some("search_files"))
            .expect("应找到 search_files 目录项");
        assert_eq!(search_en.title, "Search Files");
    }

    #[test]
    fn aggregate_sorts_chord_before_builtin_within_group() {
        // 0.21.10：组内排序 Chord > 插件 > 普通 → title——
        // Chord 项（title "Z 开头"）应排在同组普通项（title "Aa cap"）之前
        let mut chord_reg = ChordRegistry::new();
        chord_reg.register(make_capability_chord(
            "zz_chord",
            'z',
            "zz_sort_cap",
            "Z 开头",
        ));

        let cap_reg = CapabilityRegistry::default();
        cap_reg.register(make_mock_cap("aa_sort_cap")).unwrap();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        let chord_item = items
            .iter()
            .find(|i| i.source == FeatureSource::Chord && i.title == "Z 开头")
            .expect("应找到 chord 目录项");
        let builtin_item = items
            .iter()
            .find(|i| i.title == "Aa sort cap")
            .expect("应找到普通目录项");
        assert_eq!(
            chord_item.group, builtin_item.group,
            "两个未知 id 应落入同一组"
        );

        let chord_pos = items
            .iter()
            .position(|i| i.feature_id == chord_item.feature_id)
            .unwrap();
        let builtin_pos = items
            .iter()
            .position(|i| i.feature_id == builtin_item.feature_id)
            .unwrap();
        assert!(chord_pos < builtin_pos, "组内 Chord 应排在普通项之前");
    }

    #[test]
    fn chord_trigger_label_renders_space_as_text() {
        // voice_input 的 chord 键是空格——label 应显示 "Alt+Space" 而非尾随空格
        let mut chord_reg = ChordRegistry::new();
        chord_reg.register(make_voice_chord("voice_input", ' ', "语音输入"));

        let cap_reg = CapabilityRegistry::default();
        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        let voice = items
            .iter()
            .find(|i| i.feature_id == "chord.voice_input")
            .expect("应找到 voice_input 目录项");
        let label = voice
            .bindings
            .first()
            .map(|b| b.trigger_label.clone())
            .unwrap_or_default();
        assert_eq!(label, "Alt+Space");
    }

    #[test]
    fn aggregate_sorts_by_group_then_title() {
        let cap_reg = CapabilityRegistry::default();
        let chord_reg = ChordRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        // 验证排序：group 升序，组内 title 升序
        for i in 1..items.len() {
            assert!(
                items[i - 1].group <= items[i].group,
                "分组排序错误：{:?} > {:?}",
                items[i - 1].group,
                items[i].group
            );
            if items[i - 1].group == items[i].group {
                assert!(
                    items[i - 1].title <= items[i].title,
                    "组内 title 排序错误：{:?} > {:?}",
                    items[i - 1].title,
                    items[i].title
                );
            }
        }
    }

    #[test]
    fn aggregate_english_language() {
        let cap_reg = CapabilityRegistry::default();
        let chord_reg = ChordRegistry::default();

        let items_zh = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        let items_en = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "en",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        // 两语言应返回相同数量的目录项
        assert_eq!(items_zh.len(), items_en.len());

        // 英文版本的 keyword binding trigger_label 含 "Keywords:"；
        // Context 门禁动作（open_url 等）的 label 是裸 trigger key（前端翻译），
        // 不带 "Keywords:" 前缀——这是 0.21.9 的预期行为，不是回归。
        for item in &items_en {
            if let Some(sk) = item
                .bindings
                .iter()
                .find(|b| b.kind == BindingKind::SearchKeyword)
            {
                assert!(
                    sk.trigger_label.starts_with("Keywords:")
                        || sk
                            .trigger_label
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c == '_'),
                    "英文 trigger_label 应为 'Keywords:' 前缀或裸 trigger key: {}",
                    sk.trigger_label
                );
            }
        }
    }

    #[test]
    fn aggregate_gated_action_single_binding() {
        // Context 门禁动作（open_url）：关键词单独唤不起，不应展示"关键词：…"死提示。
        // 本地入口收敛为单一 action 级 binding（SearchKeyword store），label 为裸
        // trigger key（前端按 context.trigger.* 翻译为"剪贴板是 URL"）。
        let cap_reg = CapabilityRegistry::default();
        let chord_reg = ChordRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        let open_url = items
            .iter()
            .find(|i| i.feature_id == "blink.open_url")
            .expect("应找到 open_url 目录项");

        assert_eq!(open_url.bindings.len(), 1, "门禁动作应只有单一本地入口");
        let binding = &open_url.bindings[0];
        assert_eq!(binding.kind, BindingKind::SearchKeyword);
        assert_eq!(binding.binding_id, "open_url");
        assert_eq!(binding.trigger_label, "clipboard_is_url");
        assert!(binding.enabled);
        assert!(
            !open_url
                .bindings
                .iter()
                .any(|b| b.kind == BindingKind::ContextBinding),
            "门禁动作不再单列 ContextBinding（action 级开关已覆盖）"
        );
    }

    #[test]
    fn aggregate_gated_action_disabled_reflects_action_level() {
        // action 级禁用（disabled_builtin_actions）→ 单一 binding disabled + 不可用
        let cap_reg = CapabilityRegistry::default();
        let chord_reg = ChordRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &["open_url".to_string()],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        let open_url = items
            .iter()
            .find(|i| i.feature_id == "blink.open_url")
            .expect("应找到 open_url 目录项");
        assert!(!open_url.bindings[0].enabled);
        assert_eq!(open_url.local_availability, LocalAvailability::Disabled);
    }

    #[test]
    fn aggregate_keyword_action_keeps_keywords_label() {
        // 无门禁动作（open_settings）仍展示关键词提示
        let cap_reg = CapabilityRegistry::default();
        let chord_reg = ChordRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        let settings_item = items
            .iter()
            .find(|i| i.feature_id == "blink.open_settings")
            .expect("应找到 open_settings 目录项");
        let sk = settings_item
            .bindings
            .iter()
            .find(|b| b.kind == BindingKind::SearchKeyword)
            .expect("应有 SearchKeyword binding");
        assert!(
            sk.trigger_label.starts_with("关键词："),
            "{}",
            sk.trigger_label
        );
    }

    #[test]
    fn aggregate_context_binding_attached() {
        // 0.21.9：门禁动作的 context 触发已折叠进单一 action 级 binding，
        // 目录不再出现 ContextBinding kind（binding 粒度管理在 Ghost 触发规则面板）。
        let cap_reg = CapabilityRegistry::default();
        let chord_reg = ChordRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        for item in &items {
            assert!(
                !item
                    .bindings
                    .iter()
                    .any(|b| b.kind == BindingKind::ContextBinding),
                "目录不应再含 ContextBinding binding: {}",
                item.feature_id
            );
        }
    }

    #[test]
    fn aggregate_context_binding_disabled() {
        // disabled_context_bindings 不再影响目录展示（目录只投影 action 级开关；
        // context binding 粒度的启停由 Ghost 触发规则面板管理）。
        let cap_reg = CapabilityRegistry::default();
        let chord_reg = ChordRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &["builtin:open_url::clipboard_is_url".to_string()],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        let open_url = items
            .iter()
            .find(|i| i.feature_id == "blink.open_url")
            .expect("应找到 open_url 目录项");
        assert!(
            open_url.bindings[0].enabled,
            "context binding 黑名单不影响目录的 action 级开关投影"
        );
    }

    #[test]
    fn aggregate_capability_projection_fields() {
        // 注册一个有完整 policy 的 capability
        let cap_reg = CapabilityRegistry::default();
        cap_reg
            .register(make_mock_cap_with_policy(
                "proj_test_cap",
                CapabilityPolicy {
                    allowed_origins: OriginSet::ALL_LOCAL | OriginSet::CLI,
                    runtime_requirement: RuntimeRequirement::GUI_SURFACE,
                    danger: DangerClass::Dangerous,
                    sensitive: true,
                    ai_default: AiDefault::On,
                    mcp_default: McpDefault::Forbidden,
                    confirmation: ConfirmationPolicy::dangerous(false),
                },
            ))
            .unwrap();

        let chord_reg = ChordRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        let proj_item = items
            .iter()
            .find(|i| i.capability_id.as_deref() == Some("proj_test_cap"))
            .expect("应找到 proj_test_cap");

        let proj = proj_item
            .capability_projection
            .as_ref()
            .expect("应有 capability projection");

        assert_eq!(proj.danger, "dangerous");
        assert!(proj.sensitive);
        assert!(proj.requires_confirmation);
        // AI: ALL_LOCAL 含 LocalAi, ai_default=On → Enabled
        assert_eq!(proj.ai_status, CatalogExitStatus::Enabled);
        // MCP: allowed_origins 不含 Mcp → CodeForbidden
        assert_eq!(proj.mcp_status, CatalogExitStatus::CodeForbidden);
        // runtime: GUI_SURFACE normalize 后含 main_process + gui_surface
        assert!(proj.runtime_requirement.contains("gui_surface"));
    }

    #[test]
    fn aggregate_no_duplicate_feature_for_same_capability() {
        // 同一个 capability_id 不应在 descriptor 和 chord 之外再独立成项
        // 0.21.13：default() 从 inventory 收集，open_settings 已自动注册。
        // 不再手动 register 同 id mock（register 现在对重复 id 返回 Err）。
        let cap_reg = CapabilityRegistry::default();

        let chord_reg = ChordRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        // open_settings 应只出现一次（descriptor 已覆盖，不再独立成项）
        let count = items
            .iter()
            .filter(|i| i.capability_id.as_deref() == Some("open_settings"))
            .count();
        assert_eq!(count, 1, "open_settings 应只出现一次，实际 {}", count);
    }

    #[test]
    fn aggregate_chord_with_capability_not_in_descriptor() {
        // chord 指向一个不在 descriptor 中的 capability
        // 应独立成项（source = Chord）
        let mut chord_reg = ChordRegistry::new();
        chord_reg.register(make_capability_chord(
            "custom_chord",
            'x',
            "non_descriptor_cap",
            "自定义",
        ));

        let cap_reg = CapabilityRegistry::default();

        let items = FeatureCatalogAggregator::aggregate(
            &[],
            &[],
            &[],
            "zh",
            &cap_reg,
            &chord_reg,
            None,
            None,
            &HashSet::new(),
        );

        // 应有 chord.custom_chord 项
        let chord_item = items
            .iter()
            .find(|i| i.feature_id == "chord.custom_chord")
            .expect("应找到 chord.custom_chord 独立项");

        assert_eq!(chord_item.source, FeatureSource::Chord);
        assert_eq!(
            chord_item.capability_id.as_deref(),
            Some("non_descriptor_cap")
        );
    }
}
