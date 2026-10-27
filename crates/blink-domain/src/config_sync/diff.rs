//! 三方合并（共同基线 B + 本机 L + 远端 R，phase §3.5「内容冲突」）。
//!
//! 时间戳仅展示，不决定覆盖顺序。合并按 item（稳定 key）逐项进行：
//!
//! | 情况 | 处理 |
//! |---|---|
//! | 只本机改 / 只远端改 / 双方改成相同值 | 自动合并 |
//! | 同一 item 改成不同值 | 冲突，显式选择 |
//! | 删除与修改冲突（一侧缺项另一侧改） | 冲突，显式选择 |
//!
//! 有序 id 列表（weblinks/providers）的顺序是独立 `$order` item——双方都改序
//! 且不同即顺序冲突，不盲目按数组位置拼接。集合语义 item（disable 列表等）
//! 归一后比较。合并完成后 `validate_references` 做跨字段引用校验
//! （档位 → provider/model），拼不出的组合提升为关联冲突。

use serde::{Deserialize, Serialize};

use super::model::ItemMap;
use super::portable::normalize_item;

/// 冲突分类（前端按类展示文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// 同字段改成不同值
    Value,
    /// 一侧删除、另一侧修改
    DeleteVsModify,
    /// 有序列表双方改了不同顺序
    Order,
    /// 合并后跨字段引用断裂（档位指向被删 provider/model），提升为关联冲突
    Reference,
}

/// 一个待裁决冲突。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergeConflict {
    pub key: String,
    pub kind: ConflictKind,
    pub base: Option<serde_json::Value>,
    pub local: Option<serde_json::Value>,
    pub remote: Option<serde_json::Value>,
    /// Reference 冲突的人读提示（如「主档指向未同步的供应商」）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// 用户裁决。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    pub key: String,
    pub choice: ResolutionChoice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionChoice {
    Local,
    Remote,
}

/// 三方合并结果。
#[derive(Debug, Clone, Default)]
pub struct MergeResult {
    pub merged: ItemMap,
    pub conflicts: Vec<MergeConflict>,
    /// 本机新增/修改被采纳的 item 数。
    pub local_taken: usize,
    /// 远端变更被采纳的 item 数。
    pub remote_taken: usize,
}

/// 纯三方合并。base/local/remote 任一可有缺项（缺 = 删除或从未存在）。
pub fn merge_items(base: &ItemMap, local: &ItemMap, remote: &ItemMap) -> MergeResult {
    let mut keys: Vec<&String> = base.keys().chain(local.keys()).chain(remote.keys()).collect();
    keys.sort();
    keys.dedup();

    let mut result = MergeResult::default();
    for key in keys {
        let b = base.get(key.as_str());
        let l = local.get(key.as_str());
        let r = remote.get(key.as_str());
        let bn = b.map(|v| normalize_item(key, v));
        let ln = l.map(|v| normalize_item(key, v));
        let rn = r.map(|v| normalize_item(key, v));
        if ln == rn {
            // 双方一致（含双双删除）。
            if let Some(v) = l.or(r) {
                result.merged.insert(key.clone(), v.clone());
            }
        } else if ln == bn {
            // 本机未动 → 采远端（含远端删除）。
            if let Some(v) = r {
                result.merged.insert(key.clone(), v.clone());
            }
            result.remote_taken += 1;
        } else if rn == bn {
            // 远端未动 → 采本机（含本机删除）。
            if let Some(v) = l {
                result.merged.insert(key.clone(), v.clone());
            }
            result.local_taken += 1;
        } else {
            // 双方都动了且不同 → 冲突。
            let kind = if l.is_none() || r.is_none() {
                ConflictKind::DeleteVsModify
            } else if key.ends_with("$order") {
                ConflictKind::Order
            } else {
                ConflictKind::Value
            };
            result.conflicts.push(MergeConflict {
                key: key.clone(),
                kind,
                base: b.cloned(),
                local: l.cloned(),
                remote: r.cloned(),
                hint: None,
            });
        }
    }
    result
}

impl MergeResult {
    /// 应用用户裁决（未覆盖的冲突保持冲突）。返回仍未解决的冲突。
    pub fn apply_resolutions(&mut self, resolutions: &[Resolution]) -> &[MergeConflict] {
        for res in resolutions {
            if let Some(pos) = self.conflicts.iter().position(|c| c.key == res.key) {
                let conflict = self.conflicts.remove(pos);
                let chosen = match res.choice {
                    ResolutionChoice::Local => conflict.local.clone(),
                    ResolutionChoice::Remote => conflict.remote.clone(),
                };
                if let Some(v) = chosen {
                    self.merged.insert(conflict.key.clone(), v);
                } else {
                    self.merged.remove(&conflict.key);
                }
            }
        }
        &self.conflicts
    }

    /// 合并后跨字段引用校验：三档指派 → provider/model 存在性。
    /// 断裂时提升为关联冲突，候选取两侧该档位的原值（缺侧 = 删除语义），
    /// 裁决可整侧选择——不能只给个 hint 让两种选择都退化成删档位。
    /// hint 指明缺失的供应商/模型，提示用户连同上游 provider 冲突一起裁决。
    pub fn validate_references(&mut self, local: &ItemMap, remote: &ItemMap) {
        let provider_ids: Vec<String> = self
            .merged
            .keys()
            .filter_map(|k| k.strip_prefix("app.ai#providers/").map(str::to_string))
            .collect();
        let mut broken: Vec<MergeConflict> = Vec::new();
        for tier in ["tier_ultra_light", "tier_light", "tier_main"] {
            let key = format!("app.ai#{tier}");
            let Some(v) = self.merged.get(&key) else {
                continue;
            };
            let Some(pid) = v.get("provider_id").and_then(|x| x.as_str()) else {
                continue;
            };
            let mid = v.get("model_id").and_then(|x| x.as_str()).unwrap_or("");
            let model_ok = self
                .merged
                .get(&format!("app.ai#providers/{pid}"))
                .and_then(|p| p.get("models"))
                .and_then(|m| m.as_array())
                .is_some_and(|arr| arr.iter().any(|m| m.get("id").and_then(|x| x.as_str()) == Some(mid)));
            if !provider_ids.iter().any(|p| p == pid) || !model_ok {
                let missing = if !provider_ids.iter().any(|p| p == pid) {
                    format!("供应商 {pid} 不在合并结果中")
                } else {
                    format!("模型 {mid} 不在供应商 {pid} 的模型列表中")
                };
                broken.push(MergeConflict {
                    key: key.clone(),
                    kind: ConflictKind::Reference,
                    base: None,
                    local: local.get(&key).cloned(),
                    remote: remote.get(&key).cloned(),
                    hint: Some(format!(
                        "档位 {tier} 指向的{missing}；请连同相关供应商冲突一并选择保留一侧"
                    )),
                });
            }
        }
        self.conflicts.extend(broken);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(pairs: Vec<(&str, serde_json::Value)>) -> ItemMap {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    #[test]
    fn only_one_side_changed_auto_merges() {
        let base = map(vec![("a", json!(1)), ("b", json!(2)), ("c", json!(3))]);
        let local = map(vec![("a", json!(10)), ("b", json!(2)), ("c", json!(3))]);
        let remote = map(vec![("a", json!(1)), ("b", json!(20)), ("c", json!(3))]);
        let mut r = merge_items(&base, &local, &remote);
        assert!(r.conflicts.is_empty());
        assert_eq!(r.merged.get("a"), Some(&json!(10)));
        assert_eq!(r.merged.get("b"), Some(&json!(20)));
        r.apply_resolutions(&[]);
        assert_eq!(r.local_taken, 1);
        assert_eq!(r.remote_taken, 1);
    }

    #[test]
    fn same_value_both_sides_dedups() {
        let base = map(vec![("a", json!(1))]);
        let local = map(vec![("a", json!(9))]);
        let remote = map(vec![("a", json!(9))]);
        let r = merge_items(&base, &local, &remote);
        assert!(r.conflicts.is_empty());
        assert_eq!(r.merged.get("a"), Some(&json!(9)));
    }

    #[test]
    fn divergent_value_conflicts_and_resolves() {
        let base = map(vec![("theme", json!("auto"))]);
        let local = map(vec![("theme", json!("dark"))]);
        let remote = map(vec![("theme", json!("light"))]);
        let mut r = merge_items(&base, &local, &remote);
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.conflicts[0].kind, ConflictKind::Value);
        r.apply_resolutions(&[Resolution {
            key: "theme".into(),
            choice: ResolutionChoice::Remote,
        }]);
        assert!(r.conflicts.is_empty());
        assert_eq!(r.merged.get("theme"), Some(&json!("light")));
    }

    #[test]
    fn delete_vs_modify_conflicts_instead_of_guessing() {
        let base = map(vec![("entry/gh", json!({"id": "gh"}))]);
        // 远端删除，本机修改。
        let local = map(vec![("entry/gh", json!({"id": "gh", "name": "GH2"}))]);
        let remote: ItemMap = ItemMap::new();
        let mut r = merge_items(&base, &local, &remote);
        assert_eq!(r.conflicts[0].kind, ConflictKind::DeleteVsModify);
        // 选远端（确认删除）→ merged 缺项。
        r.apply_resolutions(&[Resolution {
            key: "entry/gh".into(),
            choice: ResolutionChoice::Remote,
        }]);
        assert!(!r.merged.contains_key("entry/gh"));
    }

    #[test]
    fn remote_delete_adopted_when_local_untouched() {
        let base = map(vec![("entry/gh", json!(1)), ("keep", json!(2))]);
        let local = base.clone();
        let remote = map(vec![("keep", json!(2))]);
        let r = merge_items(&base, &local, &remote);
        assert!(r.conflicts.is_empty());
        assert!(!r.merged.contains_key("entry/gh"), "未修改项的远端删除被采纳");
    }

    #[test]
    fn order_conflict_classified_separately() {
        let base = map(vec![("list$order", json!(["a", "b"]))]);
        let local = map(vec![("list$order", json!(["b", "a"]))]);
        let remote = map(vec![("list$order", json!(["a", "b", "c"]))]);
        let r = merge_items(&base, &local, &remote);
        assert_eq!(r.conflicts[0].kind, ConflictKind::Order);
    }

    #[test]
    fn set_items_compare_normalized() {
        let base = map(vec![("app.disable#disabled_builtin_actions", json!(["a"]))]);
        let local = map(vec![("app.disable#disabled_builtin_actions", json!(["b", "a", "b"]))]);
        // 远端 == 基线（未动）→ 本机集合变更直接采纳（归一后与基线不同）。
        let remote = base.clone();
        let r = merge_items(&base, &local, &remote);
        assert!(r.conflicts.is_empty());
        assert_eq!(
            r.merged.get("app.disable#disabled_builtin_actions"),
            Some(&json!(["b", "a", "b"]))
        );
        // 双方不同集合 → 值冲突。
        let remote2 = map(vec![("app.disable#disabled_builtin_actions", json!(["c"]))]);
        let r2 = merge_items(&base, &local, &remote2);
        assert_eq!(r2.conflicts.len(), 1);
    }

    #[test]
    fn reference_validation_elevates_broken_tier() {
        let mut items = map(vec![
            ("app.ai#tier_main", json!({"provider_id": "p9", "model_id": "m1"})),
            (
                "app.ai#providers/p1",
                json!({"id": "p1", "kind": "openai_compatible", "models": [{"id": "m1"}]}),
            ),
        ]);
        let mut r = MergeResult {
            merged: items.clone(),
            conflicts: vec![],
            local_taken: 0,
            remote_taken: 0,
        };
        r.validate_references(&ItemMap::new(), &ItemMap::new());
        assert!(r
            .conflicts
            .iter()
            .any(|c| c.kind == ConflictKind::Reference && c.key == "app.ai#tier_main"));
        // 合法档位不报。
        items.insert(
            "app.ai#tier_main".into(),
            json!({"provider_id": "p1", "model_id": "m1"}),
        );
        let mut r2 = MergeResult {
            merged: items,
            conflicts: vec![],
            local_taken: 0,
            remote_taken: 0,
        };
        r2.validate_references(&ItemMap::new(), &ItemMap::new());
        assert!(r2.conflicts.is_empty());
    }

    #[test]
    fn reference_conflict_carries_both_sides_and_resolves_by_choice() {
        // 场景：远端删除了 provider p1，本机 tier_main 仍指向 p1；
        // 用户对 provider 冲突选了远端（删除）→ 档位断裂提升为关联冲突。
        // 候选必须是两侧档位原值，裁决选侧才有意义（不退化成删档位）。
        let local_tier = json!({"provider_id": "p1", "model_id": "m1"});
        let remote_tier = json!({"provider_id": "p2", "model_id": "m2"});
        let mut r = MergeResult {
            merged: map(vec![
                ("app.ai#providers/p2", json!({"id": "p2", "kind": "openai_compatible", "models": [{"id": "m2"}]})),
                ("app.ai#tier_main", local_tier.clone()),
            ]),
            conflicts: vec![],
            local_taken: 0,
            remote_taken: 0,
        };
        let local = map(vec![("app.ai#tier_main", local_tier.clone())]);
        let remote = map(vec![("app.ai#tier_main", remote_tier.clone())]);
        r.validate_references(&local, &remote);
        let conflict = r.conflicts.iter().find(|c| c.kind == ConflictKind::Reference).unwrap();
        assert_eq!(conflict.local, Some(local_tier));
        assert_eq!(conflict.remote, Some(remote_tier.clone()));

        // 选远端 → merged 取远端档位（指向仍存在的 p2），不再是删除。
        r.apply_resolutions(&[Resolution {
            key: "app.ai#tier_main".into(),
            choice: ResolutionChoice::Remote,
        }]);
        assert_eq!(r.merged.get("app.ai#tier_main"), Some(&remote_tier));
        // 恢复合法引用后复验不再报冲突。
        r.validate_references(&local, &remote);
        assert!(r.conflicts.iter().all(|c| c.kind != ConflictKind::Reference));
    }

    #[test]
    fn unknown_items_merge_generically_and_are_preserved() {
        // 旧客户端不认识的新 item（远端新 schema 字段）在兼容 format_version 内
        // 照常合并并保留——不丢、不当删除。
        let base: ItemMap = ItemMap::new();
        let local: ItemMap = ItemMap::new();
        let remote = map(vec![("app.suggestion#future_field", json!(true))]);
        let r = merge_items(&base, &local, &remote);
        assert!(r.conflicts.is_empty());
        assert_eq!(r.merged.get("app.suggestion#future_field"), Some(&json!(true)));
    }
}
