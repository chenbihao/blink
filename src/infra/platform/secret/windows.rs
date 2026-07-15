//! Windows 密钥存储实现——`CredWriteW` / `CredReadW` / `CredDeleteW`（DPAPI 加密）。
//!
//! **为什么 CM 而不是 DPAPI 直接封 blob**：
//! - CM 已经封了 DPAPI + 账户级隔离，免自维护 key 文件
//! - Windows 内置 "Windows 凭据管理器" UI 让用户能看到（透明性）
//! - 卸载脚本 `CredEnumerateW` "blink/*" 一键清理
//!
//! **`CredWriteW` 参数关键**:
//! - `Type = CRED_TYPE_GENERIC` — 应用自定义密钥（不是登录凭据）
//! - `Persist = CRED_PERSIST_LOCAL_MACHINE` — 用户账户级，重启保留（不进漫游 profile）
//! - `TargetName` — 我们的 "blink/{provider_id}/{purpose}" 别名
//! - `CredentialBlob` — raw Key 字节（UTF-8）
//! - `UserName` — 空字符串（我们不用它，但 CredReadW 会填回来所以必须有效）

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use windows::Win32::Foundation::{ERROR_NOT_FOUND, FILETIME};
use windows::Win32::Security::Credentials::{
    CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
    CredReadW, CredWriteW,
};
use windows::core::PWSTR;

use super::{SecretError, SecretString, build_target_name};

/// 把 Rust `&str` 转成 CM API 需要的 null-terminated wide string。
fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 写密钥到 Credential Manager。已存在则覆盖。
///
/// **不允许日志里出现 secret**——本函数内 `tracing::` 一律不引用 `secret` 变量。
///
/// # 错误
/// - `InvalidRef`:target 拼接失败(provider_id / purpose 非法)
/// - `Platform`:`CredWriteW` 系统 API 返回失败(权限/账户异常等)
#[allow(dead_code)] // 0.9.1 Phase 2 定义,Phase 5 AI Provider 消费
pub fn save_secret(
    provider_id: &str,
    purpose: &str,
    secret: &SecretString,
) -> Result<(), SecretError> {
    let target = build_target_name(provider_id, purpose)?;

    // TargetName / UserName 用 wide string;必须活到 CredWriteW 返回后
    let mut target_w = to_wide(&target);
    // UserName 不能是 null;用空字符串占位(CredReadW 会读回)
    let mut username_w: Vec<u16> = vec![0];

    // secret bytes:CredentialBlob 是任意二进制 blob;我们存 UTF-8 编码的 Key
    // Windows 建议 UTF-16 但对 API-Key 类字符串没意义(Base64/ASCII 居多),
    // 存原始 UTF-8 更省一半空间;读的时候按 UTF-8 还原
    let secret_bytes = secret.expose().as_bytes();
    if secret_bytes.len() > 2560 {
        // CM 单条上限 2560 字节;API-Key 100~200 字节远够
        return Err(SecretError::Platform(format!(
            "密钥超过 CM 单条 2560 字节上限:实际 {} 字节",
            secret_bytes.len()
        )));
    }

    let cred = CREDENTIALW {
        Flags: CRED_FLAGS(0),
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target_w.as_mut_ptr()),
        Comment: PWSTR(ptr::null_mut()),
        LastWritten: FILETIME::default(),
        CredentialBlobSize: secret_bytes.len() as u32,
        CredentialBlob: secret_bytes.as_ptr() as *mut u8,
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: ptr::null_mut(),
        TargetAlias: PWSTR(ptr::null_mut()),
        UserName: PWSTR(username_w.as_mut_ptr()),
    };

    // Safety: `cred` 中所有指针指向的数据活到 CredWriteW 返回之后
    let result = unsafe { CredWriteW(&cred, 0) };
    result.map_err(|e| {
        // 错误信息里绝不出现 secret,只有 target 与系统 code
        SecretError::Platform(format!("CredWriteW 失败(target={target}): {}", e.code()))
    })?;

    tracing::debug!(target = %target, "密钥已写入 Credential Manager");
    Ok(())
}

/// 从 Credential Manager 读密钥。
///
/// **读回来的字节立即包进 `SecretString`**——不给中间态明文一秒钟的暴露窗口。
///
/// # 错误
/// - `NotFound`:target 不存在(用户没配 / 已删)
/// - `Platform`:其他系统错误
#[allow(dead_code)] // 0.9.1 Phase 2 定义,Phase 5 AI Provider 消费
pub fn load_secret(provider_id: &str, purpose: &str) -> Result<SecretString, SecretError> {
    let target = build_target_name(provider_id, purpose)?;
    let target_w = to_wide(&target);

    let mut cred_ptr: *mut CREDENTIALW = ptr::null_mut();

    // Safety: PCWSTR 指向 target_w 有效切片;out ptr 由 CM 分配,必须 CredFree
    let result = unsafe {
        CredReadW(
            windows::core::PCWSTR(target_w.as_ptr()),
            CRED_TYPE_GENERIC,
            None,
            &mut cred_ptr,
        )
    };

    if let Err(e) = result {
        if e.code() == windows::core::HRESULT::from_win32(ERROR_NOT_FOUND.0) {
            return Err(SecretError::NotFound(target));
        }
        return Err(SecretError::Platform(format!(
            "CredReadW 失败(target={target}): {}",
            e.code()
        )));
    }

    if cred_ptr.is_null() {
        return Err(SecretError::Platform("CredReadW 返回 null".to_string()));
    }

    // Safety: cred_ptr 非空且指向 CM 分配的合法 CREDENTIALW。
    // 这里只把 blob 拷成 Vec<u8>——拿到副本后立即 CredFree,绝不把可能失败的
    // 解析(UTF-8 校验)留在 unsafe 块里:否则 from_utf8 失败提前返回会跳过
    // CredFree,泄漏 CM 分配的 CREDENTIALW。
    let blob_bytes: Vec<u8> = unsafe {
        let cred = &*cred_ptr;
        let blob_len = cred.CredentialBlobSize as usize;
        let blob_slice = if cred.CredentialBlob.is_null() || blob_len == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(cred.CredentialBlob, blob_len)
        };
        blob_slice.to_vec()
    };

    // 释放 CM 分配的内存——拿到字节副本后就不再需要原 blob
    unsafe { CredFree(cred_ptr as _) };

    // UTF-8 还原——写入时严格 UTF-8;若 blob 被外部篡改/损坏成非法 UTF-8,
    // 显式报 Platform 而非静默替换成 U+FFFD(否则后续 API 调用 401 且无从定位)。
    let raw = String::from_utf8(blob_bytes)
        .map_err(|_| SecretError::Platform("密钥 blob 不是合法 UTF-8(可能已损坏)".to_string()))?;
    let secret_string = SecretString::new(raw);

    // 日志里不引 secret 内容——只报"读到了,长度多少"
    tracing::debug!(target = %target, bytes = secret_string.len(), "密钥已从 CM 读回");
    Ok(secret_string)
}

/// 从 Credential Manager 删除密钥。
///
/// # 错误
/// - `NotFound`:target 不存在(通常表示已删,视为幂等——调用方可选择忽略)
/// - `Platform`:其他系统错误
#[allow(dead_code)] // 0.9.1 Phase 2 定义,Phase 5 AI Provider 消费
pub fn delete_secret(provider_id: &str, purpose: &str) -> Result<(), SecretError> {
    let target = build_target_name(provider_id, purpose)?;
    let target_w = to_wide(&target);

    // Safety: PCWSTR 指向 target_w 有效切片
    let result = unsafe {
        CredDeleteW(
            windows::core::PCWSTR(target_w.as_ptr()),
            CRED_TYPE_GENERIC,
            None,
        )
    };

    if let Err(e) = result {
        if e.code() == windows::core::HRESULT::from_win32(ERROR_NOT_FOUND.0) {
            return Err(SecretError::NotFound(target));
        }
        return Err(SecretError::Platform(format!(
            "CredDeleteW 失败(target={target}): {}",
            e.code()
        )));
    }

    tracing::debug!(target = %target, "密钥已从 CM 删除");
    Ok(())
}

// ── 单测:三链路(写→读→删),依赖真实 Windows Credential Manager ────────────
//
// **可跳过守卫**:CI 无桌面 session 时 CM API 会失败;`Path::exists` 惯用法在
// 这里不适用,改用"先尝试写一次能不能过"作为运行环境探针。
//
// 断言"写完能读到 & 读到的原文一致 & 删完再读拿到 NotFound"——三条走通即
// 证明骨架条 #6"SecretString 生命周期干净"落地。

#[cfg(test)]
mod tests {
    use super::*;

    /// 探测:当前环境能否用 Credential Manager。CI headless 环境会拒绝。
    fn cm_available() -> bool {
        // 用一个不会冲突的探测别名快速试写→删
        let probe_target = "blink/__cm_probe__/probe";
        let probe_w = to_wide(probe_target);
        let mut target_w = probe_w.clone();
        let mut username_w: Vec<u16> = vec![0];
        let blob = b"probe";
        let cred = CREDENTIALW {
            Flags: CRED_FLAGS(0),
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target_w.as_mut_ptr()),
            Comment: PWSTR(ptr::null_mut()),
            LastWritten: FILETIME::default(),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_ptr() as *mut u8,
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: ptr::null_mut(),
            TargetAlias: PWSTR(ptr::null_mut()),
            UserName: PWSTR(username_w.as_mut_ptr()),
        };
        let ok = unsafe { CredWriteW(&cred, 0) }.is_ok();
        if ok {
            let _ = unsafe {
                CredDeleteW(
                    windows::core::PCWSTR(probe_w.as_ptr()),
                    CRED_TYPE_GENERIC,
                    None,
                )
            };
        }
        ok
    }

    #[test]
    fn write_read_delete_roundtrip() {
        if !cm_available() {
            eprintln!("跳过:当前环境不可用 Credential Manager");
            return;
        }

        // 用带随机后缀的 provider_id 避免测试并发冲突
        let provider = format!("test-{}", std::process::id());
        let secret_raw = "sk-blink-test-1234567890abcdef";
        let secret = SecretString::new(secret_raw.to_string());

        // 1. 写
        save_secret(&provider, "key", &secret).expect("save 应成功");

        // 2. 读——原文必须一致
        let loaded = load_secret(&provider, "key").expect("load 应成功");
        assert_eq!(loaded.expose(), secret_raw, "读回原文不匹配");

        // 3. 覆盖写——同别名再写不同 Key,读回必须是新的
        let secret2 = SecretString::new("sk-blink-updated-abcxyz".to_string());
        save_secret(&provider, "key", &secret2).expect("覆盖写应成功");
        let loaded2 = load_secret(&provider, "key").expect("覆盖后 load 应成功");
        assert_eq!(loaded2.expose(), "sk-blink-updated-abcxyz");

        // 4. 删
        delete_secret(&provider, "key").expect("delete 应成功");

        // 5. 删后读——必须拿到 NotFound
        match load_secret(&provider, "key") {
            Err(SecretError::NotFound(_)) => {}
            other => panic!("删后 load 应返回 NotFound,实际:{other:?}"),
        }

        // 6. 幂等性:再删一次(不存在)——必须是 NotFound 而不是 Platform
        match delete_secret(&provider, "key") {
            Err(SecretError::NotFound(_)) => {}
            other => panic!("重复 delete 应返回 NotFound,实际:{other:?}"),
        }
    }

    #[test]
    fn load_missing_returns_not_found() {
        if !cm_available() {
            eprintln!("跳过:当前环境不可用 Credential Manager");
            return;
        }
        // 大概率不存在的别名
        let provider = format!("test-nonexistent-{}", std::process::id());
        match load_secret(&provider, "key") {
            Err(SecretError::NotFound(_)) => {}
            other => panic!("读不存在的别名应返回 NotFound,实际:{other:?}"),
        }
    }

    #[test]
    fn save_rejects_oversized_secret() {
        // 2561 字节 = 超过 CM 单条上限 1 字节
        let big = "a".repeat(2561);
        let secret = SecretString::new(big);
        let provider = format!("test-oversize-{}", std::process::id());
        match save_secret(&provider, "key", &secret) {
            Err(SecretError::Platform(msg)) => {
                assert!(msg.contains("2560"), "错误消息应提到上限,实际: {msg}");
                assert!(!msg.contains("aaaaaa"), "错误消息绝不能含密钥字节");
            }
            other => panic!("超长密钥应被拒绝,实际:{other:?}"),
        }
    }
}
