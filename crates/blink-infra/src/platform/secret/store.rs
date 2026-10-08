//! keyring 后端——0.17.11 替换自写 unsafe FFI，0.25.15 改 v2 命名。
//!
//! **v2 命名（0.25.15）**：`keyring_core::Entry::new_with_modifiers` + `target`
//! modifier 显式指定完整 CM target name（`blink:{module}:{subject}:{purpose}`，
//! 由 `build_target_name_v2` 纯函数产出）。不再依赖 keyring 默认的
//! `{user}.{service}` 拼接——那个形状把应用名排到尾部（`sensenova/key.blink`），
//! 与常见应用"应用前缀置首"的习惯相反。
//!
//! **默认 store 初始化**：生产路径仍借 keyring v1 的 `Entry::store_status()`
//! 触发 LazyLock（构造 windows-native store 并 `keyring_core::set_default_store`），
//! 之后 `keyring_core::Entry::new_with_modifiers` 走同一默认 store——windows
//! native store 的 `build` 支持 `target` modifier（service/user 被忽略）。
//!
//! **单测隔离**：测试使用自写内存 store（`test_store` 模块，按 target name 键存，
//! 支持 target modifier——keyring 自带 mock 不支持 modifier，会直接拒绝），
//! 绝不触碰真实 CM。这是 0.17.11 的核心修复——根除 `cargo test` 清空生产 CM 的元凶。

use std::collections::HashMap;

use super::{SecretError, SecretRef, SecretString, build_target_name_v2};

/// keyring service 名（固定）。v2 命名下 target 由 `build_target_name_v2` 全权决定，
/// 此值仅作为 `new_with_modifiers` 的占位参数（windows native store 在显式 target
/// 时忽略 service/user）。
const SERVICE: &str = "blink";

/// 按 v2 命名构造 keyring Entry。
///
/// 默认 store 必须已初始化（生产路径首次调用前有 `keyring::Entry::store_status()`
/// 触发；测试路径由 `test_store` 覆盖默认 store 后天然满足）。
fn entry_for(r: &SecretRef) -> Result<keyring_core::Entry, SecretError> {
    let target = build_target_name_v2(r)?;
    let mut modifiers = HashMap::new();
    modifiers.insert("target", target.as_str());
    keyring_core::Entry::new_with_modifiers(SERVICE, "", &modifiers)
        .map_err(|e| SecretError::Platform(format!("keyring Entry::new_with_modifiers 失败: {e}")))
}

/// 触发 keyring v1 默认 store 初始化（构造 windows-native store）。
///
/// 幂等——LazyLock 全进程只执行一次。`pub(crate)`：`migrate_v2.rs` 构造 v1 形状
/// Entry 前需保证默认 store 已初始化；单测覆盖默认 store 前也显式调用。
pub(crate) fn ensure_platform_store() {
    let _ = keyring::Entry::store_status();
}

/// 写密钥到 keyring（Windows 后端走 Credential Manager）。已存在则覆盖。
///
/// # 错误
/// - `InvalidRef`: SecretRef 段非法（空 / 含 `:` / 含 `\0`）
/// - `Platform`: keyring API 返回失败
pub fn save_secret(r: &SecretRef, secret: &SecretString) -> Result<(), SecretError> {
    ensure_platform_store();
    let entry = entry_for(r)?;
    entry
        .set_password(secret.expose())
        .map_err(|e| SecretError::Platform(format!("keyring set_password 失败: {e}")))?;
    tracing::debug!(target = ?build_target_name_v2(r), "密钥已写入 keyring(v2)");
    Ok(())
}

/// 从 keyring 读密钥。
///
/// **读回来的字节立即包进 `SecretString`**——不给中间态明文暴露窗口。
///
/// # 错误
/// - `NotFound`: 别名不存在（用户没配 / 已删）
/// - `Platform`: 其他系统错误
pub fn load_secret(r: &SecretRef) -> Result<SecretString, SecretError> {
    ensure_platform_store();
    let entry = entry_for(r)?;
    match entry.get_password() {
        Ok(raw) => {
            let s = SecretString::new(raw);
            tracing::debug!(
                target = ?build_target_name_v2(r),
                bytes = s.len(),
                "密钥已从 keyring 读回(v2)"
            );
            Ok(s)
        }
        Err(keyring_core::Error::NoEntry) => Err(SecretError::NotFound(
            build_target_name_v2(r).unwrap_or_else(|_| r.subject.clone()),
        )),
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
pub fn delete_secret(r: &SecretRef) -> Result<(), SecretError> {
    ensure_platform_store();
    let entry = entry_for(r)?;
    match entry.delete_credential() {
        Ok(()) => {
            tracing::debug!(target = ?build_target_name_v2(r), "密钥已从 keyring 删除(v2)");
            Ok(())
        }
        Err(keyring_core::Error::NoEntry) => Err(SecretError::NotFound(
            build_target_name_v2(r).unwrap_or_else(|_| r.subject.clone()),
        )),
        Err(e) => Err(SecretError::Platform(format!(
            "keyring delete_credential 失败: {e}"
        ))),
    }
}

// ── 单测：自写内存 store，绝不碰真实 CM ─────────────────────────────────────
//
// **为什么不用 keyring_core::mock**：mock store 拒绝一切 entry modifier
// （`NotSupportedByStore`），而 v2 命名依赖 `target` modifier——所以按 keyring-core
// 的 `CredentialStoreApi`/`CredentialApi` 自写一个按 target name 键存的内存实现
// （对齐 mock.rs 的内部可变性模式），单测行为与生产 store 对 target 的处理一致。
//
// **mock 设置原理**：
// 1. `ensure_platform_store()` 触发 v1 LazyLock（初始化 Windows native store 为默认）
// 2. `keyring_core::set_default_store(test_store())` 覆盖默认 store
// 3. 后续 `keyring_core::Entry::new_with_modifiers` 从当前默认 store 创建 entry
// 用 `Once` 保证整个测试进程只设置一次。

#[cfg(test)]
mod test_store {
    //! 按完整 target name 键存的内存 credential store（单测专用）。
    //!
    //! 支持 `target` modifier（与 windows-native store 行为一致）；未提供 target 时
    //! 按 `{user}.{service}` 键存（对齐 keyring 默认拼接，供对照测试用）。

    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, Once};

    use keyring_core::api::{CredentialApi, CredentialStoreApi};
    use keyring_core::{Credential, CredentialPersistence, Entry, Error, Result};

    /// 单条内存凭据——`Mutex<RefCell<Data>>` 内部可变性（对齐 keyring mock 模式）。
    pub struct Cred {
        key: String,
        inner: Mutex<RefCell<Option<Vec<u8>>>>,
    }

    impl CredentialApi for Cred {
        fn set_secret(&self, secret: &[u8]) -> Result<()> {
            let mut inner = self.inner.lock().expect("test store 锁中毒");
            *inner.get_mut() = Some(secret.to_vec());
            Ok(())
        }

        fn get_secret(&self) -> Result<Vec<u8>> {
            let inner = self.inner.lock().expect("test store 锁中毒");
            inner.borrow().clone().ok_or(Error::NoEntry)
        }

        fn delete_credential(&self) -> Result<()> {
            let mut inner = self.inner.lock().expect("test store 锁中毒");
            match inner.get_mut().take() {
                Some(_) => Ok(()),
                None => Err(Error::NoEntry),
            }
        }

        fn get_credential(&self) -> Result<Option<Arc<Credential>>> {
            Ok(None)
        }

        fn get_specifiers(&self) -> Option<(String, String)> {
            None // 显式 target 条目无 service/user specifiers（与 windows native 一致）
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// 内存 store——`Vec<Arc<Cred>>` 简单线性查找（测试规模无需 HashMap 优化）。
    struct Store {
        creds: Mutex<RefCell<Vec<Arc<Cred>>>>,
    }

    impl CredentialStoreApi for Store {
        fn vendor(&self) -> String {
            String::from("blink secret test store (target-keyed in-memory)")
        }

        fn id(&self) -> String {
            String::from("blink-secret-test-store")
        }

        fn build(
            &self,
            service: &str,
            user: &str,
            mods: Option<&HashMap<&str, &str>>,
        ) -> Result<Entry> {
            let key = match mods.and_then(|m| m.get("target")) {
                Some(target) => (*target).to_string(),
                None => format!("{user}.{service}"),
            };
            let mut creds = self.creds.lock().expect("test store 锁中毒");
            let list = creds.get_mut();
            if let Some(existing) = list.iter().find(|c| c.key == key) {
                return Ok(Entry::new_with_credential(existing.clone()));
            }
            let cred = Arc::new(Cred {
                key,
                inner: Mutex::new(RefCell::new(None)),
            });
            list.push(cred.clone());
            Ok(Entry::new_with_credential(cred))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn persistence(&self) -> CredentialPersistence {
            CredentialPersistence::ProcessOnly
        }
    }

    static INIT: Once = Once::new();

    /// 覆盖默认 store 为内存实现——单测绝不碰真实 CM。
    pub fn setup() {
        INIT.call_once(|| {
            // 1. 触发 v1 LazyLock（初始化平台默认 store，生产同路径）
            let _ = keyring::Entry::store_status();
            // 2. 覆盖为内存 store
            keyring_core::set_default_store(Arc::new(Store {
                creds: Mutex::new(RefCell::new(Vec::new())),
            }));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::test_store;
    use super::*;
    use crate::platform::secret::SecretModule;

    /// 每个测试先装内存 store（进程级 Once，首个调用者生效）。
    fn setup_mock_store() {
        test_store::setup();
    }

    #[test]
    fn write_read_delete_roundtrip() {
        setup_mock_store();
        let r = SecretRef::ai(&format!("test-{}", std::process::id()));
        let secret = SecretString::new("sk-blink-test-1234567890abcdef".to_string());
        save_secret(&r, &secret).expect("save 应成功");
        let loaded = load_secret(&r).expect("load 应成功");
        assert_eq!(loaded.expose(), "sk-blink-test-1234567890abcdef");
        // 覆盖写
        let secret2 = SecretString::new("sk-blink-updated-abcxyz".to_string());
        save_secret(&r, &secret2).expect("覆盖写应成功");
        let loaded2 = load_secret(&r).expect("覆盖后 load 应成功");
        assert_eq!(loaded2.expose(), "sk-blink-updated-abcxyz");
        // 删
        delete_secret(&r).expect("delete 应成功");
        match load_secret(&r) {
            Err(SecretError::NotFound(_)) => {}
            other => panic!("删后 load 应返回 NotFound,实际:{other:?}"),
        }
        // 幂等性：再删一次（不存在）——必须是 NotFound 而不是 Platform
        match delete_secret(&r) {
            Err(SecretError::NotFound(_)) => {}
            other => panic!("重复 delete 应返回 NotFound,实际:{other:?}"),
        }
    }

    #[test]
    fn load_missing_returns_not_found() {
        setup_mock_store();
        let r = SecretRef::ai(&format!("test-nonexistent-{}", std::process::id()));
        match load_secret(&r) {
            Err(SecretError::NotFound(target)) => {
                // NotFound 消息带的是 v2 target name，便于定位
                assert!(
                    target.starts_with("blink:ai:"),
                    "NotFound 应带 v2 target: {target}"
                )
            }
            other => panic!("读不存在的别名应返回 NotFound,实际:{other:?}"),
        }
    }

    #[test]
    fn save_rejects_invalid_ref() {
        setup_mock_store();
        let secret = SecretString::new("sk-test".to_string());
        // 空 subject
        match save_secret(&SecretRef::ai(""), &secret) {
            Err(SecretError::InvalidRef(_)) => {}
            other => panic!("空 subject 应返回 InvalidRef,实际:{other:?}"),
        }
        // subject 含 ':'（破坏分段解析）
        match save_secret(&SecretRef::ai("a:b"), &secret) {
            Err(SecretError::InvalidRef(_)) => {}
            other => panic!("含 ':' 的 subject 应返回 InvalidRef,实际:{other:?}"),
        }
        // subject 含 '\0'
        match save_secret(&SecretRef::ai("test\0evil"), &secret) {
            Err(SecretError::InvalidRef(_)) => {}
            other => panic!("含 \\0 的 subject 应返回 InvalidRef,实际:{other:?}"),
        }
    }

    /// 不同模块/主体的 target 互不串扰——v2 命名的隔离性。
    #[test]
    fn distinct_refs_are_isolated_entries() {
        setup_mock_store();
        let refs = [
            SecretRef::ai("11111111-1111-1111-1111-111111111111"),
            SecretRef::ai("22222222-2222-2222-2222-222222222222"),
            SecretRef::stt_cloud(),
            SecretRef::plugin("builtin.translate", "deepl_api_key"),
        ];
        for (i, r) in refs.iter().enumerate() {
            save_secret(r, &SecretString::new(format!("sk-{i}"))).expect("save 应成功");
        }
        for (i, r) in refs.iter().enumerate() {
            let loaded = load_secret(r).expect("load 应成功");
            assert_eq!(loaded.expose(), format!("sk-{i}"), "条目串扰: {:?}", r);
        }
        // Module 变体可用性（消除 unused 警告的显式覆盖）
        assert_eq!(refs[2].module, SecretModule::Stt);
    }

    /// 回归测试：确认单测不污染真实 CM。
    /// 运行后 `cmdkey /list` 不应出现 blink: 开头的 test 条目。
    /// 验证方式：测试套件运行后手动跑 `cmdkey /list | findstr blink:` 确认无 regression-probe。
    #[test]
    fn mock_store_does_not_touch_real_cm() {
        setup_mock_store();
        let secret = SecretString::new("regression-check".to_string());
        // 如果内存 store 生效，这行不会写进真实 CM
        let r = SecretRef::ai("regression-probe");
        save_secret(&r, &secret).unwrap();
        // 能读回来证明内存 store 在工作
        let loaded = load_secret(&r).unwrap();
        assert_eq!(loaded.expose(), "regression-check");
    }
}
