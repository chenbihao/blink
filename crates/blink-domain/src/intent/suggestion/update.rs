//! UpdateProducer（0.26.20 更新提醒）：后台静默检查发现新版本时空 query 提示。
//!
//! **Producer 纯度铁则**（0.24 §3.7）：本 producer 不发网络请求、不读配置库——
//! 「可用更新」由 app 层在唤起采集后盖章进 `AwarenessSnapshot.available_update`
//! （三条件：自动检查开 && `latest > current` && 未 dismiss），此处纯读快照。
//!
//! **空 query 独占**：非空 query 恒不产——用户输入即意图表达，版本提示不进
//! 输入路径。槽位竞争由 Coordinator 特判：`UpdateAvailable` 不进正常排序池，
//! 仅在 primary/secondary 空缺时填充（不挤占翻译/上下文等更高优先级建议），
//! 且不受降频抑制（显式 dismiss 是唯一消失通道）。

use blink_infra::platform::context::AwarenessSnapshot;

use super::producer::SuggestionProducer;
use super::{Suggestion, SuggestionAction, SuggestionKind, SuggestionSource, text_fingerprint};

/// 更新提醒 producer。无内部状态——版本号全部来自快照。
pub struct UpdateProducer;

impl SuggestionProducer for UpdateProducer {
    fn source(&self) -> SuggestionSource {
        SuggestionSource::Update
    }

    #[allow(deprecated)] // 构造 Suggestion 时填充 ranking_hint: None（§3.7 冻结 produce 签名，过渡通道保留）
    fn produce(&self, query: &str, snapshot: &AwarenessSnapshot) -> Vec<Suggestion> {
        if !query.trim().is_empty() {
            return Vec::new();
        }
        let Some(version) = snapshot.available_update.as_deref() else {
            return Vec::new();
        };
        vec![Suggestion {
            id: "update-available".to_string(),
            kind: SuggestionKind::UpdateAvailable,
            // 采纳 = 跳设置关于 tab（复用 changelog 分组 + 一键更新完整 UI）；
            // 前端对 UpdateAvailable 采纳同时记录 dismissed_version（用户已知情）
            action: SuggestionAction::OpenSettingsTab {
                tab: "about".to_string(),
            },
            // 低分仅表意——实际槽位由 Coordinator 空缺填充特判决定，不参与排序竞争
            rank_score: 0.5,
            display: version.to_string(),
            prefix_len: 0,
            origin: None,
            fingerprint: text_fingerprint(version),
            source: SuggestionSource::Update,
            ranking_hint: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap_with_update(version: Option<&str>) -> AwarenessSnapshot {
        let mut snap = AwarenessSnapshot::default();
        snap.available_update = version.map(|v| v.to_string());
        snap
    }

    /// 空 query + 快照带版本 → 单条 UpdateAvailable 建议，action 跳设置 about。
    #[test]
    fn empty_query_with_update_yields_suggestion() {
        let p = UpdateProducer;
        let snap = snap_with_update(Some("0.26.21"));
        let out = p.produce("", &snap);
        assert_eq!(out.len(), 1);
        let s = &out[0];
        assert_eq!(s.id, "update-available");
        assert_eq!(s.kind, SuggestionKind::UpdateAvailable);
        assert_eq!(s.display, "0.26.21");
        assert_eq!(
            s.action,
            SuggestionAction::OpenSettingsTab {
                tab: "about".to_string()
            }
        );
        assert_eq!(s.source, SuggestionSource::Update);
    }

    /// 非空 query（含纯空格外的任何输入）恒不产——版本提示不进输入路径。
    #[test]
    fn non_empty_query_yields_nothing() {
        let p = UpdateProducer;
        let snap = snap_with_update(Some("0.26.21"));
        assert!(p.produce("chrome", &snap).is_empty());
        assert!(p.produce("更", &snap).is_empty());
    }

    /// 快照无版本（未检测/已 dismiss/开关关）→ 不产。
    #[test]
    fn no_update_yields_nothing() {
        let p = UpdateProducer;
        assert!(p.produce("", &snap_with_update(None)).is_empty());
    }
}
