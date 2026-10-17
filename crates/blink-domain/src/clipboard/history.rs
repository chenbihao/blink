//! 剪贴板历史的类型化身份与批量结果。大载荷留在所属存储，不进入选择/菜单协议。
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardEntryKind {
    Text,
    Image,
    /// 文件路径条目（0.26.9）：路径列表存 `clipboard_files`，内容不入库。
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipboardEntryRef {
    pub kind: ClipboardEntryKind,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardEntryOutcome {
    pub entry: ClipboardEntryRef,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn validate_entries(entries: Vec<ClipboardEntryRef>) -> Result<Vec<ClipboardEntryRef>, String> {
    if entries.is_empty() || entries.len() > 2000 {
        return Err("一次操作须包含 1–2000 条剪贴板记录".into());
    }
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for entry in entries {
        if entry.id.is_empty() || entry.id.len() > 512 {
            return Err("剪贴板记录 ID 无效".into());
        }
        if seen.insert(entry.clone()) {
            unique.push(entry);
        }
    }
    Ok(unique)
}

/// 各独立用户数据库分别提交事务，跨库失败显式返回，不能报告整体回滚。
///
/// 0.26.9：文件条目（`clipboard_files`）与文本同在 history 库，但独立表独立事务。
pub async fn delete_entries(
    pools: &blink_infra::data::DbPools,
    entries: Vec<ClipboardEntryRef>,
) -> Vec<ClipboardEntryOutcome> {
    let texts: Vec<&str> = entries
        .iter()
        .filter(|r| r.kind == ClipboardEntryKind::Text)
        .map(|r| r.id.as_str())
        .collect();
    let images: Vec<&str> = entries
        .iter()
        .filter(|r| r.kind == ClipboardEntryKind::Image)
        .map(|r| r.id.as_str())
        .collect();
    let files: Vec<&str> = entries
        .iter()
        .filter(|r| r.kind == ClipboardEntryKind::File)
        .map(|r| r.id.as_str())
        .collect();
    let text_error = blink_infra::data::clipboard::delete_items_by_ids(&pools.history, &texts)
        .await
        .err()
        .map(|e| e.to_string());
    let image_error =
        blink_infra::data::clipboard_images::delete_images_by_ids(&pools.cache, &images)
            .await
            .err()
            .map(|e| e.to_string());
    let file_error = blink_infra::data::clipboard_files::delete_files_by_ids(&pools.history, &files)
        .await
        .err()
        .map(|e| e.to_string());
    entries
        .into_iter()
        .map(|entry| {
            let error = match entry.kind {
                ClipboardEntryKind::Text => text_error.clone(),
                ClipboardEntryKind::Image => image_error.clone(),
                ClipboardEntryKind::File => file_error.clone(),
            };
            ClipboardEntryOutcome {
                entry,
                success: error.is_none(),
                error,
            }
        })
        .collect()
}

/// 顺序消费图片：每次只加载/提交一张，失败不阻断其他项，位置只随成功推进。
pub async fn pin_entries<F, Fut>(
    entries: Vec<ClipboardEntryRef>,
    mut pin: F,
) -> Vec<ClipboardEntryOutcome>
where
    F: FnMut(ClipboardEntryRef, Option<(i32, i32)>) -> Fut,
    Fut: std::future::Future<Output = Result<(i32, i32), String>>,
{
    let mut position: Option<(i32, i32)> = None;
    let mut outcomes = Vec::new();
    for entry in entries {
        let next = position.map(|(x, y)| (x.saturating_add(24), y.saturating_add(24)));
        let error = match pin(entry.clone(), next).await {
            Ok(actual) => {
                position = Some(actual);
                None
            }
            Err(error) => Some(error),
        };
        outcomes.push(ClipboardEntryOutcome {
            entry,
            success: error.is_none(),
            error,
        });
        tokio::task::yield_now().await;
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_is_typed_and_dedup_preserves_order() {
        let text = ClipboardEntryRef {
            kind: ClipboardEntryKind::Text,
            id: "same".into(),
        };
        let image = ClipboardEntryRef {
            kind: ClipboardEntryKind::Image,
            id: "same".into(),
        };
        assert_eq!(
            validate_entries(vec![image.clone(), text.clone(), image.clone()]).unwrap(),
            vec![image, text]
        );
        assert!(
            serde_json::from_value::<ClipboardEntryRef>(
                serde_json::json!({"kind":"audio","id":"x"})
            )
            .is_err()
        );
        // 0.26.9：file 变体的 wire 形态（snake_case）
        let file: ClipboardEntryRef =
            serde_json::from_value(serde_json::json!({"kind":"file","id":"clipfile_1"})).unwrap();
        assert_eq!(file.kind, ClipboardEntryKind::File);
        assert!(validate_entries(vec![]).is_err());
    }

    async fn pools() -> blink_infra::data::DbPools {
        let history = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let cache = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        blink_infra::data::DbPools {
            config: history.clone(),
            ai: history.clone(),
            history,
            cache,
        }
    }

    #[tokio::test]
    async fn mixed_delete_reports_committed_and_failed_databases_separately() {
        let pools = pools().await;
        sqlx::query("CREATE TABLE clipboard_history (id TEXT PRIMARY KEY)")
            .execute(&pools.history)
            .await
            .unwrap();
        sqlx::query("INSERT INTO clipboard_history VALUES ('same')")
            .execute(&pools.history)
            .await
            .unwrap();
        // 图片库故意缺表：文本库成功提交，不得误报跨库整体回滚或全部成功。
        let refs = vec![
            ClipboardEntryRef {
                kind: ClipboardEntryKind::Image,
                id: "same".into(),
            },
            ClipboardEntryRef {
                kind: ClipboardEntryKind::Text,
                id: "same".into(),
            },
        ];
        let outcomes = delete_entries(&pools, refs.clone()).await;
        assert!(!outcomes[0].success);
        assert!(outcomes[0].error.is_some());
        assert!(outcomes[1].success);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM clipboard_history")
            .fetch_one(&pools.history)
            .await
            .unwrap();
        assert_eq!(count, 0);
        sqlx::query("CREATE TABLE clipboard_images (id TEXT PRIMARY KEY)")
            .execute(&pools.cache)
            .await
            .unwrap();
        assert!(
            delete_entries(&pools, refs)
                .await
                .iter()
                .all(|outcome| outcome.success),
            "重复删除已缺失记录幂等成功"
        );
    }

    #[tokio::test]
    async fn image_transaction_rolls_back_every_delete_on_failure() {
        let pools = pools().await;
        sqlx::query("CREATE TABLE clipboard_images (id TEXT PRIMARY KEY)")
            .execute(&pools.cache)
            .await
            .unwrap();
        sqlx::query("INSERT INTO clipboard_images VALUES ('a'), ('b')")
            .execute(&pools.cache)
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER reject_b BEFORE DELETE ON clipboard_images WHEN OLD.id = 'b' BEGIN SELECT RAISE(ABORT, 'blocked'); END")
            .execute(&pools.cache).await.unwrap();
        assert!(
            blink_infra::data::clipboard_images::delete_images_by_ids(&pools.cache, &["a", "b"])
                .await
                .is_err()
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM clipboard_images")
            .fetch_one(&pools.cache)
            .await
            .unwrap();
        assert_eq!(count, 2, "第一条删除必须随同回滚");
        assert!(
            blink_infra::data::clipboard_images::delete_image(&pools.cache, "b")
                .await
                .is_err(),
            "单删同样传播错误"
        );
    }

    #[tokio::test]
    async fn pin_batch_continues_after_failure_and_offsets_only_successful_images() {
        let refs = ["first", "missing", "last"].map(|id| ClipboardEntryRef {
            kind: ClipboardEntryKind::Image,
            id: id.into(),
        });
        let seen = std::cell::RefCell::new(Vec::new());
        let outcomes = pin_entries(refs.to_vec(), |entry, position| {
            seen.borrow_mut().push((entry.id.clone(), position));
            std::future::ready(if entry.id == "missing" {
                Err("missing image".into())
            } else {
                Ok(position.unwrap_or((100, 200)))
            })
        })
        .await;
        assert_eq!(
            outcomes.iter().map(|o| o.success).collect::<Vec<_>>(),
            vec![true, false, true]
        );
        assert_eq!(
            *seen.borrow(),
            vec![
                ("first".into(), None),
                ("missing".into(), Some((124, 224))),
                ("last".into(), Some((124, 224)))
            ]
        );
    }
}
