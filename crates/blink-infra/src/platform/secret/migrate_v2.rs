//! v1 命名（`{pid}/{purpose}.blink`）→ v2 命名（`blink:{module}:{subject}:{purpose}`）
//! 的启动期自动收编（0.25.15；0.25.16-fix 改自愈式，见下）。
//!
//! **迁移步骤**（每次启动执行，幂等由构造保证）：
//! 1. `enumerate_blink_secrets_anywhere()` 枚举 target 含 `blink` 的条目
//!    （全量枚举 + Rust 端过滤，覆盖 v0 残留、v1 `…/key.blink` 与已是 v2 的 `blink:…`）。
//! 2. 对每个 target 用 `parse_keyring_v1_target` 识别 v1 形状（`{pid}/{purpose}.blink`）：
//!    - 解析出 `(pid, purpose)` → `SecretRef::from_legacy_pid` 恢复模块归属
//!      （`stt:cloud` → Stt，其余 → Ai——含 0.14.6–0.17.11 的 slug id 如 `sensenova`）。
//!    - **经 keyring Entry（v1 命名形状）读明文 → `store::save_secret` 写 v2 →
//!      经同一 Entry 删老条目**。读写老条目必须走 keyring 路径：v1 条目由 keyring
//!      Windows 后端写入，CredentialBlob 是 UTF-16LE；`windows_legacy::load_secret_raw`
//!      按 UTF-8 解码会把 `sk-abc` 读成 `s\0k\0…` 乱码并静默写入 v2（v0 条目是
//!      自写 FFI 的 UTF-8，两者编码不同——这正是 v0/v1 迁移各用各读取路径的原因）。
//!    - 非 v1 形状（v0 残留由 `migrate.rs` 负责；v2 已迁移）跳过，仅 debug 日志。
//!
//! **为什么无 marker（0.25.16-fix 修订）**：首版用 marker 门控一次性执行，但枚举
//! 走了 `CredEnumerateW("*blink*")`——实测该 API 的通配语法**不支持前导+尾随通配**
//! （`*blink*` 直接 ERROR_NOT_FOUND，被当"无条目"），迁移静默空跑还照写 marker，
//! 用户全部 AI 密钥显示未配置。修订为：① 枚举改全量 + Rust 端 contains 过滤
//! （`enumerate_blink_secrets_anywhere`，不再依赖 CM 通配语法）；② 扫描每启动执行
//! ——一次全量枚举（数十条量级，亚毫秒）远比"marker 写了但没收编"的静默失败便宜，
//! 自愈还顺带覆盖双版本并行、外部恢复备份等边角。无 v1 残留时扫描自然 no-op。
//!
//! **失败处理**：全程不 panic——枚举失败/单条读写失败均 `tracing::warn!` 后继续；
//! 失败条目保留 v1 原状（无损），下次启动扫描自动重试。
//!
//! **跳过版本链说明**：用户从 0.17.11 前直接升到 0.25.15+ 时，`migrate.rs`（v0→新命名）
//! 先执行——它经 `store::save_secret` 落地即已是 v2 命名，本迁移随后枚举不到 v1 条目。
//! 两代迁移任意起点都收敛到 v2。

use crate::platform::secret::SecretRef;

/// keyring v1 命名的 service 名与 target 后缀（target 形如 `{pid}/{purpose}.blink`）。
const V1_SERVICE: &str = "blink";
const V1_SUFFIX: &str = ".blink";

/// 解析 keyring v1 target name（`{pid}/{purpose}.blink`）为 `(pid, purpose)`。
///
/// 纯函数——单测覆盖三类真实形状（UUID / slug / stt:cloud）与各反例。
/// 返回 `None` 表示不是 v1 形状（v0 `blink/…` 残留或已是 v2），调用方跳过。
pub fn parse_keyring_v1_target(target: &str) -> Option<(String, String)> {
    let user = target.strip_suffix(V1_SUFFIX)?;
    // v1 的 user = `{pid}/{purpose}`，必含 `/`；v2 命名（`blink:…`）与
    // v0 残留（`blink/{pid}/{purpose}`，不以 .blink 结尾——purpose 恒为 "key"）不会走到这里
    let (pid, purpose) = user.rsplit_once('/')?;
    if pid.is_empty() || purpose.is_empty() {
        return None;
    }
    Some((pid.to_string(), purpose.to_string()))
}

/// 构造指向 v1 命名条目的 keyring Entry（迁移读写老条目专用）。
///
/// 无 modifier 的 `Entry::new(service, user)` 在 Windows 后端生成 `{user}.{service}`
/// target——正是 v1 写入时的形状，读/删与写入路径完全对称（含 UTF-16 blob 编解码）。
fn v1_entry(pid: &str, purpose: &str) -> Result<keyring_core::Entry, String> {
    crate::platform::secret::store::ensure_platform_store();
    let user = format!("{pid}/{purpose}");
    keyring_core::Entry::new(V1_SERVICE, &user)
        .map_err(|e| format!("keyring Entry::new(v1) 失败: {e}"))
}

/// 收编 CM 中残留的 v1 命名密钥到 v2 命名（每次启动执行，幂等，自愈）。
///
/// 无 v1 残留时为一次全量枚举的 no-op（亚毫秒级）；写 v2 成功后才删老条目，
/// 单向流动，失败则保留老条目下次启动自动重试。
pub async fn migrate_keyring_v1_to_v2() {
    use crate::platform::secret::enumerate_blink_secrets_anywhere;

    // 枚举含 blink 的条目（全量枚举 + Rust 端过滤——不依赖 CredEnumerateW 通配语法）
    let candidates = match enumerate_blink_secrets_anywhere() {
        Ok(secrets) => secrets,
        Err(e) => {
            // 不写任何状态——下次启动重试（无 marker，天然自愈）
            tracing::warn!(error = %e, "密钥收编:枚举 CM 失败,本次跳过");
            return;
        }
    };

    let mut migrated = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

    for info in &candidates {
        let target = &info.target_name;

        // 识别 v1 形状
        let Some((pid, purpose)) = parse_keyring_v1_target(target) else {
            skipped += 1;
            continue;
        };

        // 经 keyring Entry（v1 形状）读老条目明文——UTF-16 解码与写入路径对称
        let entry = match v1_entry(&pid, &purpose) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(target = %target, error = %e, "密钥收编:构造 v1 Entry 失败,保留老条目");
                failed += 1;
                continue;
            }
        };
        match entry.get_password() {
            Ok(raw) => {
                let secret = crate::platform::secret::SecretString::new(raw);
                // 写入 v2 命名（from_legacy_pid 恢复模块归属：stt:cloud → Stt，其余 → Ai）
                let r = SecretRef::from_legacy_pid(&pid, &purpose);
                match crate::platform::secret::save_secret(&r, &secret) {
                    Ok(()) => {
                        // 经同一 Entry 删老条目（写成功后才删,单向流动）
                        if let Err(e) = entry.delete_credential()
                            && !matches!(e, keyring_core::Error::NoEntry)
                        {
                            tracing::warn!(
                                target = %target,
                                error = %e,
                                "密钥收编:删老条目失败(新条目已写,无害)"
                            );
                        }
                        migrated += 1;
                    }
                    Err(e) => {
                        tracing::warn!(target = %target, error = %e, "密钥收编:写 v2 失败,保留老条目");
                        failed += 1;
                    }
                }
            }
            Err(keyring_core::Error::NoEntry) => {
                // 老条目读不到（可能已损坏/被外部删）,跳过
                tracing::debug!(target = %target, "密钥收编:老条目读不到,跳过");
            }
            Err(e) => {
                tracing::warn!(target = %target, error = %e, "密钥收编:读老条目失败,保留");
                failed += 1;
            }
        }
    }

    if migrated > 0 || failed > 0 {
        tracing::info!(
            migrated,
            failed,
            skipped,
            total = candidates.len(),
            "密钥收编完成(v1→v2)"
        );
    }
}

#[cfg(test)]
mod tests {
    // 迁移主流程依赖真实 CM,属于集成路径,不自动化（与 migrate.rs 同策略）。
    // 纯逻辑部分（v1 target 解析）单测覆盖。

    use super::parse_keyring_v1_target as parse;

    #[test]
    fn parse_v1_uuid_provider() {
        assert_eq!(
            parse("550e8400-e29b-41d4-a716-446655440000/key.blink"),
            Some(("550e8400-e29b-41d4-a716-446655440000".into(), "key".into()))
        );
    }

    #[test]
    fn parse_v1_slug_provider() {
        // 0.14.6–0.17.11 期间的显示名 slug id
        assert_eq!(
            parse("sensenova/key.blink"),
            Some(("sensenova".into(), "key".into()))
        );
    }

    #[test]
    fn parse_v1_stt_alias() {
        assert_eq!(
            parse("stt:cloud/key.blink"),
            Some(("stt:cloud".into(), "key".into()))
        );
    }

    #[test]
    fn parse_rejects_non_v1_shapes() {
        // v2 命名（无 .blink 后缀）
        assert_eq!(parse("blink:ai:550e8400:key"), None);
        // v0 残留（blink/ 前缀,不以 .blink 结尾）
        assert_eq!(parse("blink/sensenova/key"), None);
        // 无 / 分段
        assert_eq!(parse("sensenova.blink"), None);
        // 空 pid / 空 purpose
        assert_eq!(parse("/key.blink"), None);
        assert_eq!(parse("pid/.blink"), None);
        // 恰好以 .blink 结尾的 v0 伪装形状（blink/…/key.blink）——pid="blink" 会被
        // from_legacy_pid 误判为 Ai 模块;此形状在真实历史中不存在,但解析上仍按
        // v1 处理(返回 Some),迁移会把它搬到 v2 命名下,无损
    }
}
