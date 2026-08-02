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
    CRED_ENUMERATE_FLAGS, CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW,
    CredDeleteW, CredEnumerateW, CredFree, CredReadW, CredWriteW,
};
use windows::core::PWSTR;

use super::{SecretError, SecretInfo, SecretString, build_target_name};

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

// ── 批量枚举与清理（0.16.6）──────────────────────────────────────────────────

/// 枚举 Credential Manager 中所有 `blink/*` 命名空间的密钥元信息。
///
/// 使用 `CredEnumerateW` 的通配符过滤（`"blink*"`），返回 target name 列表。
/// **不返回密钥内容**——只供清理展示与确认。
///
/// # 错误
/// - `Platform`:`CredEnumerateW` 系统 API 返回失败
pub fn enumerate_blink_secrets() -> Result<Vec<SecretInfo>, SecretError> {
    let filter = to_wide("blink*");
    let mut count: u32 = 0;
    let mut creds_ptr: *mut *mut CREDENTIALW = ptr::null_mut();

    // Safety: filter 指向有效的 wide string;count/creds_ptr 是栈上合法输出参数
    let result = unsafe {
        CredEnumerateW(
            windows::core::PCWSTR(filter.as_ptr()),
            Some(CRED_ENUMERATE_FLAGS(0)),
            &mut count,
            &mut creds_ptr,
        )
    };

    if let Err(e) = result {
        // ERROR_NOT_FOUND 表示没有匹配的凭据——返回空列表而非报错
        if e.code() == windows::core::HRESULT::from_win32(ERROR_NOT_FOUND.0) {
            return Ok(Vec::new());
        }
        return Err(SecretError::Platform(format!(
            "CredEnumerateW 失败: {}",
            e.code()
        )));
    }

    if creds_ptr.is_null() || count == 0 {
        return Ok(Vec::new());
    }

    // Safety: creds_ptr 非空且指向 CM 分配的 count 个指针数组。
    // 每个 *mut CREDENTIALW 指向 CM 分配的合法结构体。
    // 只读取 TargetName 字段（PWSTR），拷贝为 String 后立即 CredFree 整个数组。
    let mut secrets = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let cred_ptr = unsafe { *creds_ptr.add(i) };
        if cred_ptr.is_null() {
            continue;
        }
        let cred = unsafe { &*cred_ptr };
        // TargetName 是 PWSTR，指向 CM 分配的 wide string
        let target_name = if cred.TargetName.is_null() {
            String::new()
        } else {
            unsafe { cred.TargetName.to_string().unwrap_or_default() }
        };
        if !target_name.is_empty() {
            secrets.push(SecretInfo { target_name });
        }
    }

    // 释放 CM 分配的凭据数组
    unsafe { CredFree(creds_ptr as _) };

    tracing::debug!(count = secrets.len(), "枚举到 blink/* 密钥条目");
    Ok(secrets)
}

/// 批量删除 Credential Manager 中所有 `blink/*` 命名空间的密钥。
///
/// 返回每个 target 的删除结果，调用方可按项展示成功/失败。
/// `NotFound` 视为成功（幂等——可能被其他途径已删）。
///
/// # 返回
/// `Vec<(target_name, Result<(), SecretError>)>`
pub fn delete_all_blink_secrets() -> Vec<(String, Result<(), SecretError>)> {
    let secrets = match enumerate_blink_secrets() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "delete_all_blink_secrets: 枚举失败，无法批量删除");
            return vec![("<enumerate_failed>".to_string(), Err(e))];
        }
    };

    let mut results = Vec::with_capacity(secrets.len());
    for info in &secrets {
        let target = &info.target_name;
        let target_w = to_wide(target);

        // Safety: PCWSTR 指向 target_w 有效切片
        let result = unsafe {
            CredDeleteW(
                windows::core::PCWSTR(target_w.as_ptr()),
                CRED_TYPE_GENERIC,
                None,
            )
        };

        match result {
            Ok(()) => {
                tracing::debug!(target = %target, "批量删除密钥成功");
                results.push((target.clone(), Ok(())));
            }
            Err(e) => {
                if e.code() == windows::core::HRESULT::from_win32(ERROR_NOT_FOUND.0) {
                    // NotFound 视为成功（幂等）
                    results.push((target.clone(), Ok(())));
                } else {
                    tracing::warn!(target = %target, error = %e, "批量删除密钥失败");
                    results.push((
                        target.clone(),
                        Err(SecretError::Platform(format!(
                            "CredDeleteW 失败(target={target}): {}",
                            e.code()
                        ))),
                    ));
                }
            }
        }
    }

    tracing::info!(
        total = results.len(),
        failed = results.iter().filter(|(_, r)| r.is_err()).count(),
        "批量删除 blink/* 密钥完成"
    );
    results
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

    #[test]
    fn enumerate_and_delete_all_blink_secrets() {
        if !cm_available() {
            eprintln!("跳过:当前环境不可用 Credential Manager");
            return;
        }

        // 写入两条 blink/* 密钥
        let p1 = format!("test-enum-{}", std::process::id());
        let p2 = format!("test-enum2-{}", std::process::id());
        let s1 = SecretString::new("sk-enum-test-1".to_string());
        let s2 = SecretString::new("sk-enum-test-2".to_string());
        save_secret(&p1, "key", &s1).expect("写 secret 1 应成功");
        save_secret(&p2, "key", &s2).expect("写 secret 2 应成功");

        // 枚举——应该包含我们刚写的两条（至少）
        let secrets = enumerate_blink_secrets().expect("枚举应成功");
        let has_p1 = secrets.iter().any(|s| s.target_name.contains(&p1));
        let has_p2 = secrets.iter().any(|s| s.target_name.contains(&p2));
        assert!(has_p1, "枚举结果应包含刚写入的 secret 1");
        assert!(has_p2, "枚举结果应包含刚写入的 secret 2");

        // 批量删除
        let results = delete_all_blink_secrets();
        // 每条结果应该都是 Ok（NotFound 也视为成功）
        for (target, result) in &results {
            assert!(result.is_ok(), "删除 {target} 应成功");
        }
        // 验证刚写的两条已被删除
        assert!(
            results.iter().any(|(t, _)| t.contains(&p1)),
            "删除结果应包含 secret 1"
        );
        assert!(
            results.iter().any(|(t, _)| t.contains(&p2)),
            "删除结果应包含 secret 2"
        );

        // 再次枚举——不应包含测试密钥
        let after = enumerate_blink_secrets().expect("二次枚举应成功");
        assert!(
            !after.iter().any(|s| s.target_name.contains(&p1)),
            "删除后不应再枚举到 secret 1"
        );
        assert!(
            !after.iter().any(|s| s.target_name.contains(&p2)),
            "删除后不应再枚举到 secret 2"
        );

        // 二次删除——幂等，不应报错
        let _ = delete_all_blink_secrets();
    }
}
