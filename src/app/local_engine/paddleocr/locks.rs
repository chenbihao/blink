//! PaddleOCR 依赖锁解析：嵌入的 `locked-requirements.txt` → `PackageLock` 列表
//! （安装时的唯一锁源，不再手写第二份包清单）。

use crate::infra::local_engine::providers::PackageLock;

use super::LOCKED_REQUIREMENTS_TXT;

/// 解析 `locked-requirements.txt` 格式的依赖锁文件。
///
/// 格式（`uv pip compile --generate-hashes` 输出）：
/// ```text
/// # comment lines
/// package-name==1.2.3 \
///     --hash=sha256:abcdef... \
///     --hash=sha256:123456...
/// ```
///
/// 每个包可能有多个 hash（对应不同平台的 wheel）。
/// 对于 `--require-hashes` 安装，需要列出所有 hash 让 pip 匹配。
///
/// 返回 `Vec<PackageLock>`，每个包的 `sha256` 为第一个 hash（用于摘要/标识），
/// `all_hashes` 包含所有平台 wheel 的 hash，用于 `--require-hashes` 安装。
pub(super) fn parse_locked_requirements(txt: &str) -> Vec<PackageLock> {
    let mut packages = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_version: Option<String> = None;
    let mut current_hashes: Vec<String> = Vec::new();

    for line in txt.lines() {
        let trimmed = line.trim();

        // Skip comments and empty lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Hash continuation line
        if trimmed.starts_with("--hash=sha256:") {
            let h = trimmed
                .trim_start_matches("--hash=sha256:")
                .trim_end_matches('\\')
                .trim();
            if !h.is_empty() {
                current_hashes.push(h.to_string());
            }
            continue;
        }

        // New package line: contains ==
        if trimmed.contains("==") {
            // Save previous package
            if let (Some(name), Some(version)) = (&current_name, &current_version) {
                let first_hash = current_hashes.first().cloned();
                packages.push(PackageLock {
                    name: name.clone(),
                    version: version.clone(),
                    sha256: first_hash,
                    all_hashes: current_hashes.clone(),
                });
            }

            // Parse new package: strip trailing backslash
            let line_clean = trimmed.trim_end_matches('\\').trim();
            if let Some(eq_pos) = line_clean.find("==") {
                let name = line_clean[..eq_pos].trim().to_string();
                let version_part = &line_clean[eq_pos + 2..];
                // Version may have trailing space or hash on same line
                let version = version_part
                    .split_whitespace()
                    .next()
                    .unwrap_or(version_part)
                    .to_string();
                current_name = Some(name);
                current_version = Some(version);
                current_hashes.clear();

                // Check if there's a hash on the same line
                if let Some(hash_start) = trimmed.find("--hash=sha256:") {
                    let h = trimmed[hash_start..]
                        .trim_start_matches("--hash=sha256:")
                        .trim_end_matches('\\')
                        .trim();
                    if !h.is_empty() {
                        current_hashes.push(h.to_string());
                    }
                }
            }
        }
    }

    // Save last package
    if let (Some(name), Some(version)) = (&current_name, &current_version) {
        let first_hash = current_hashes.first().cloned();
        packages.push(PackageLock {
            name: name.clone(),
            version: version.clone(),
            sha256: first_hash,
            all_hashes: current_hashes.clone(),
        });
    }

    packages
}

/// 从嵌入的 `locked-requirements.txt` 解析包列表。
///
/// 这是安装时使用的唯一锁源——不再手写第二份包清单。
pub(super) fn locked_packages() -> Vec<PackageLock> {
    let packages = parse_locked_requirements(LOCKED_REQUIREMENTS_TXT);
    // 验证：所有包必须有 hash
    for pkg in &packages {
        assert!(
            pkg.sha256.is_some(),
            "locked-requirements.txt 中的 {} 缺少 SHA-256 hash",
            pkg.name
        );
        let hash = pkg.sha256.as_ref().unwrap();
        assert_eq!(
            hash.len(),
            64,
            "locked-requirements.txt 中的 {} 的 hash 长度不是 64: {}",
            pkg.name,
            hash
        );
        assert!(
            hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "locked-requirements.txt 中的 {} 的 hash 包含非 hex 字符: {}",
            pkg.name,
            hash
        );
        // all_hashes 不得为空
        assert!(
            !pkg.all_hashes.is_empty(),
            "locked-requirements.txt 中的 {} 的 all_hashes 为空",
            pkg.name
        );
        // all_hashes 中每个 hash 也必须格式正确
        for h in &pkg.all_hashes {
            assert_eq!(
                h.len(),
                64,
                "locked-requirements.txt 中的 {} 的 all_hashes 中有长度不为 64 的 hash",
                pkg.name
            );
            assert!(
                h.bytes().all(|b| b.is_ascii_hexdigit()),
                "locked-requirements.txt 中的 {} 的 all_hashes 中有非 hex 字符",
                pkg.name
            );
        }
    }
    packages
}
