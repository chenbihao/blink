//! CM 老命名（`blink/{id}/{purpose}`）→ keyring 新命名的一次性迁移（0.17.11）。
//!
//! 仿 `app_config.rs` 的单 key 迁移模式 + `config.rs` 的 marker 幂等模式。
//!
//! **迁移步骤**：
//! 1. 检查 marker（config 表 key = `secrets_keyring_migrated_done`）；存在则跳过。
//! 2. 调 `windows_legacy::enumerate_blink_secrets()` 枚举老 `blink/*` 条目。
//! 3. 对每个 target（形如 `blink/sensenova/key` 或 `blink/stt:cloud/key`）：
//!    - 解析出 `(provider_id, purpose)`：`strip_prefix("blink/")` 后 `rsplit_once('/')`。
//!    - 用 `windows_legacy::load_secret_raw(target)` 读明文。
//!    - 用 `store::save_secret(pid, purpose, &secret)` 写入 keyring 新命名。
//!    - 删老 CM 条目（`windows_legacy::delete_secret_raw(target)`）。
//! 4. 写 marker `secrets_keyring_migrated_done = "1"`。
//!
//! **失败处理**：全程不 panic——枚举失败/单条读写失败均 `tracing::warn!` 后继续。
//! marker 只在全部处理完写。中途 panic 下次启动重试（已迁移的条目重复写是幂等的）。

use sqlx::SqlitePool;

/// 迁移 marker key——存在表示迁移已完成。
const MARKER_KEY: &str = "secrets_keyring_migrated_done";

/// 执行 CM 老命名 → keyring 新命名的一次性迁移。
///
/// 幂等：marker 存在则直接返回。迁移过程中单条失败不阻断——写 keyring 成功后才删老条目，
/// 单向流动，失败则保留老条目下次重试（但 marker 仍写入，避免无限重试已损坏的条目）。
pub async fn migrate_legacy_cm_to_keyring(pool: &SqlitePool) {
    use crate::infra::data::config::{get_config, set_config};
    use crate::infra::platform::secret::windows_legacy;

    // 1. 检查 marker——已迁移则跳过
    if get_config(pool, MARKER_KEY).await.is_some() {
        return;
    }

    tracing::info!("开始执行 CM→keyring 密钥迁移");

    // 2. 枚举老 CM blink/* 条目
    let legacy = match windows_legacy::enumerate_blink_secrets() {
        Ok(secrets) => secrets,
        Err(e) => {
            tracing::warn!(error = %e, "迁移:枚举老 CM 失败,跳过(下次启动重试)");
            return;
        }
    };

    if legacy.is_empty() {
        tracing::info!("迁移:老 CM 无 blink/* 条目,直接写 marker");
        let _ = set_config(pool, MARKER_KEY, "1").await;
        return;
    }

    let mut migrated = 0usize;
    let mut failed = 0usize;

    for info in &legacy {
        let target = &info.target_name; // 形如 "blink/sensenova/key" 或 "blink/stt:cloud/key"

        // 3a. 解析 (provider_id, purpose)
        let Some(rest) = target.strip_prefix("blink/") else {
            tracing::warn!(target = %target, "迁移:target 不以 blink/ 开头,跳过");
            continue;
        };
        let Some((pid, purpose)) = rest.rsplit_once('/') else {
            tracing::warn!(target = %target, "迁移:target 格式异常(无 purpose 段),跳过");
            continue;
        };

        // 3b. 从老 CM 读明文（用 raw 接口,直接按 target 读）
        match windows_legacy::load_secret_raw(target) {
            Ok(secret) => {
                // 3c. 写入 keyring 新命名
                match crate::infra::platform::secret::save_secret(pid, purpose, &secret) {
                    Ok(()) => {
                        // 3d. 删老 CM 条目（写成功后才删,单向流动）
                        if let Err(e) = windows_legacy::delete_secret_raw(target) {
                            tracing::warn!(
                                target = %target,
                                error = %e,
                                "迁移:删老 CM 条目失败(新条目已写,无害)"
                            );
                        }
                        migrated += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            target = %target,
                            error = %e,
                            "迁移:写 keyring 失败,保留老条目"
                        );
                        failed += 1;
                    }
                }
            }
            Err(crate::infra::platform::secret::SecretError::NotFound(_)) => {
                // 老条目读不到（可能已损坏/被外部删）,跳过
                tracing::debug!(target = %target, "迁移:老条目读不到,跳过");
            }
            Err(e) => {
                tracing::warn!(
                    target = %target,
                    error = %e,
                    "迁移:读老 CM 失败,保留老条目"
                );
                failed += 1;
            }
        }
    }

    tracing::info!(
        migrated,
        failed,
        total = legacy.len(),
        "CM→keyring 密钥迁移完成"
    );

    // 4. 写 marker（无论有无失败都写——失败的条目下次也不会成功,避免无限重试）
    if let Err(e) = set_config(pool, MARKER_KEY, "1").await {
        tracing::warn!(error = %e, "迁移:marker 写入失败,下次启动会重试");
    }
}

#[cfg(test)]
mod tests {
    // 迁移函数的纯逻辑部分（target 解析）可单测,不依赖真实 CM。
    // 完整迁移流程依赖真实 CM + keyring,属于集成测试范畴,不自动化。

    use super::MARKER_KEY;

    #[test]
    fn marker_key_is_stable() {
        // marker key 必须稳定——改了会导致迁移重复执行或跳过
        assert_eq!(MARKER_KEY, "secrets_keyring_migrated_done");
    }

    #[test]
    fn parse_legacy_target_ai_provider() {
        // 标准格式:blink/{provider_id}/key
        let target = "blink/sensenova/key";
        let rest = target.strip_prefix("blink/").unwrap();
        let (pid, purpose) = rest.rsplit_once('/').unwrap();
        assert_eq!(pid, "sensenova");
        assert_eq!(purpose, "key");
    }

    #[test]
    fn parse_legacy_target_stt() {
        // STT 格式:blink/stt:cloud/key
        let target = "blink/stt:cloud/key";
        let rest = target.strip_prefix("blink/").unwrap();
        let (pid, purpose) = rest.rsplit_once('/').unwrap();
        assert_eq!(pid, "stt:cloud");
        assert_eq!(purpose, "key");
    }

    #[test]
    fn parse_legacy_target_uuid() {
        // UUID 格式:blink/{uuid}/key
        let target = "blink/550e8400-e29b-41d4-a716-446655440000/key";
        let rest = target.strip_prefix("blink/").unwrap();
        let (pid, purpose) = rest.rsplit_once('/').unwrap();
        assert_eq!(pid, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(purpose, "key");
    }

    #[test]
    fn parse_legacy_target_rejects_no_prefix() {
        // 不以 blink/ 开头
        let target = "other/foo/key";
        assert!(target.strip_prefix("blink/").is_none());
    }

    #[test]
    fn parse_legacy_target_rejects_no_purpose() {
        // 没有 purpose 段（只有一个 /）
        let target = "blink/sensenova";
        let rest = target.strip_prefix("blink/").unwrap();
        assert!(rest.rsplit_once('/').is_none());
    }
}
