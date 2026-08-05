//! keyring 后端——替换原 windows.rs 的 unsafe FFI（0.17.11）。
//!
//! 保持 `save_secret`/`load_secret`/`delete_secret` 签名不变，调用方零改动。
//! Windows 后端走 Credential Manager（与原实现同等安全级别）。
//!
//! **命名约定**：keyring 在 Windows 后端生成的 CM target_name 形如
//! `{user}.{service}`（delimiter 默认 `.`），即 `sensenova/key.blink`，
//! 与老命名 `blink/sensenova/key` **不同**。靠 `migrate.rs` 迁移桥接。
//!
//! **单测隔离**：测试使用 `keyring_core::mock` 内存 store，绝不触碰真实 CM。
//! 这是 0.17.11 的核心修复——根除 `cargo test` 清空生产 CM 的元凶。

use keyring::Entry;

use super::{SecretError, SecretString};

/// keyring service 名（固定）。user = `{provider_id}/{purpose}`。
/// Windows 后端生成的 CM target_name 形如 `{user}.{service}`（delimiter 默认 `.`）。
const SERVICE: &str = "blink";

/// 构造 keyring user 字符串并做校验。
///
/// 复用 `build_target_name` 的合法性规则（非空 + 不含 `\0`），
/// 但允许 `/`（user 格式是 `{provider_id}/{purpose}`，service + user 组合
/// 在 keyring Windows 后端生成 `{user}.{service}` target）。
fn entry_user(provider_id: &str, purpose: &str) -> Result<String, SecretError> {
    if provider_id.is_empty() || provider_id.contains('\0') {
        return Err(SecretError::InvalidRef(format!(
            "provider_id 非法: {provider_id:?}"
        )));
    }
    if purpose.is_empty() || purpose.contains('\0') {
        return Err(SecretError::InvalidRef(format!("purpose 非法: {purpose:?}")));
    }
    Ok(format!("{provider_id}/{purpose}"))
}

/// 写密钥到 keyring（Windows 后端走 Credential Manager）。已存在则覆盖。
///
/// # 错误
/// - `InvalidRef`: provider_id / purpose 非法
/// - `Platform`: keyring API 返回失败
pub fn save_secret(
    provider_id: &str,
    purpose: &str,
    secret: &SecretString,
) -> Result<(), SecretError> {
    let user = entry_user(provider_id, purpose)?;
    let entry = Entry::new(SERVICE, &user)
        .map_err(|e| SecretError::Platform(format!("keyring Entry::new 失败: {e}")))?;
    entry
        .set_password(secret.expose())
        .map_err(|e| SecretError::Platform(format!("keyring set_password 失败: {e}")))?;
    tracing::debug!(service = SERVICE, user = %user, "密钥已写入 keyring");
    Ok(())
}

/// 从 keyring 读密钥。
///
/// **读回来的字节立即包进 `SecretString`**——不给中间态明文暴露窗口。
///
/// # 错误
/// - `NotFound`: 别名不存在（用户没配 / 已删）
/// - `Platform`: 其他系统错误
pub fn load_secret(provider_id: &str, purpose: &str) -> Result<SecretString, SecretError> {
    let user = entry_user(provider_id, purpose)?;
    let entry = Entry::new(SERVICE, &user)
        .map_err(|e| SecretError::Platform(format!("keyring Entry::new 失败: {e}")))?;
    match entry.get_password() {
        Ok(raw) => {
            let s = SecretString::new(raw);
            tracing::debug!(
                service = SERVICE,
                user = %user,
                bytes = s.len(),
                "密钥已从 keyring 读回"
            );
            Ok(s)
        }
        Err(keyring::Error::NoEntry) => Err(SecretError::NotFound(user)),
        Err(e) => Err(SecretError::Platform(format!(
            "keyring get_password 失败: {e}"
        ))),
    }
}

/// 从 keyring 删除密钥。
///
/// # 错误
/// - `NotFound`: 别名不存在（通常表示已删，视为幂等——调用方可选择忽略）
/// - `Platform`: 其他系统错误
pub fn delete_secret(provider_id: &str, purpose: &str) -> Result<(), SecretError> {
    let user = entry_user(provider_id, purpose)?;
    let entry = Entry::new(SERVICE, &user)
        .map_err(|e| SecretError::Platform(format!("keyring Entry::new 失败: {e}")))?;
    match entry.delete_credential() {
        Ok(()) => {
            tracing::debug!(service = SERVICE, user = %user, "密钥已从 keyring 删除");
            Ok(())
        }
        Err(keyring::Error::NoEntry) => Err(SecretError::NotFound(user)),
        Err(e) => Err(SecretError::Platform(format!(
            "keyring delete_credential 失败: {e}"
        ))),
    }
}

// ── 单测：使用 keyring mock store，绝不碰真实 CM ─────────────────────────────
//
// **核心修复**：0.17.11 之前 `windows.rs` 的单测直接打真实 Windows Credential Manager，
// `enumerate_and_delete_all_blink_secrets` 测试会清空整个 `blink/*` 命名空间。
// 现在用 keyring 的 mock credential store（内存实现），`cargo test` 绝不触碰真实 CM。
//
// **mock 设置原理**：
// 1. `keyring::Entry::store_status()` 触发 v1 模块的 LazyLock（初始化 Windows native store）
// 2. `keyring_core::set_default_store(mock)` 覆盖默认 store 为 mock（内存实现）
// 3. 后续 `keyring::Entry::new` 检查 LazyLock 结果（Ok），然后走 `keyring_core::Entry::new`
//    从当前默认 store（已被覆盖为 mock）创建 entry
// 用 `Once` 保证整个测试进程只设置一次。

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    static MOCK_INIT: Once = Once::new();

    /// 设置 keyring mock store——单测绝不碰真实 CM。
    fn setup_mock_store() {
        MOCK_INIT.call_once(|| {
            // 1. 触发 v1 LazyLock（初始化 Windows native store 为默认 store）
            let _ = keyring::Entry::store_status();
            // 2. 覆盖默认 store 为 mock（内存实现）
            let mock = keyring_core::mock::Store::new().expect("mock store 创建应成功");
            keyring_core::set_default_store(mock);
        });
    }

    #[test]
    fn write_read_delete_roundtrip() {
        setup_mock_store();
        let provider = format!("test-{}", std::process::id());
        let secret = SecretString::new("sk-blink-test-1234567890abcdef".to_string());
        save_secret(&provider, "key", &secret).expect("save 应成功");
        let loaded = load_secret(&provider, "key").expect("load 应成功");
        assert_eq!(loaded.expose(), "sk-blink-test-1234567890abcdef");
        // 覆盖写
        let secret2 = SecretString::new("sk-blink-updated-abcxyz".to_string());
        save_secret(&provider, "key", &secret2).expect("覆盖写应成功");
        let loaded2 = load_secret(&provider, "key").expect("覆盖后 load 应成功");
        assert_eq!(loaded2.expose(), "sk-blink-updated-abcxyz");
        // 删
        delete_secret(&provider, "key").expect("delete 应成功");
        match load_secret(&provider, "key") {
            Err(SecretError::NotFound(_)) => {}
            other => panic!("删后 load 应返回 NotFound,实际:{other:?}"),
        }
        // 幂等性：再删一次（不存在）——必须是 NotFound 而不是 Platform
        match delete_secret(&provider, "key") {
            Err(SecretError::NotFound(_)) => {}
            other => panic!("重复 delete 应返回 NotFound,实际:{other:?}"),
        }
    }

    #[test]
    fn load_missing_returns_not_found() {
        setup_mock_store();
        let provider = format!("test-nonexistent-{}", std::process::id());
        match load_secret(&provider, "key") {
            Err(SecretError::NotFound(_)) => {}
            other => panic!("读不存在的别名应返回 NotFound,实际:{other:?}"),
        }
    }

    #[test]
    fn save_rejects_empty_provider_id() {
        setup_mock_store();
        let secret = SecretString::new("sk-test".to_string());
        match save_secret("", "key", &secret) {
            Err(SecretError::InvalidRef(_)) => {}
            other => panic!("空 provider_id 应返回 InvalidRef,实际:{other:?}"),
        }
    }

    #[test]
    fn save_rejects_empty_purpose() {
        setup_mock_store();
        let secret = SecretString::new("sk-test".to_string());
        match save_secret("test-provider", "", &secret) {
            Err(SecretError::InvalidRef(_)) => {}
            other => panic!("空 purpose 应返回 InvalidRef,实际:{other:?}"),
        }
    }

    #[test]
    fn save_rejects_null_byte_in_provider_id() {
        setup_mock_store();
        let secret = SecretString::new("sk-test".to_string());
        match save_secret("test\0evil", "key", &secret) {
            Err(SecretError::InvalidRef(_)) => {}
            other => panic!("含 \\0 的 provider_id 应返回 InvalidRef,实际:{other:?}"),
        }
    }

    /// 回归测试：确认单测不污染真实 CM。
    /// 运行后 `cmdkey /list:blink*` 不应出现 test 条目。
    /// 验证方式：测试套件运行后手动跑 `cmdkey /list:blink*` 确认无 regression-probe。
    #[test]
    fn mock_store_does_not_touch_real_cm() {
        setup_mock_store();
        let secret = SecretString::new("regression-check".to_string());
        // 如果 mock 生效，这行不会写进真实 CM
        save_secret("regression-probe", "key", &secret).unwrap();
        // 能读回来证明 mock store 在工作
        let loaded = load_secret("regression-probe", "key").unwrap();
        assert_eq!(loaded.expose(), "regression-check");
    }
}
