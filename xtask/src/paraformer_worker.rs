//! `cargo xtask paraformer-worker` — ParaformerOnline worker 供应链校验（0.22.9）。
//!
//! ParaformerOnline worker 不是独立编译的二进制——它是 `blink.exe` 自身的
//! 一个隐藏子进程入口（`blink.exe paraformer-selftest` + 启动时的
//! `ManagedProcess` spawn）。因此本命令不编译 C/C++ 代码，而是：
//!
//! 1. 校验 STT asset-lock.json 可解析且结构完整
//! 2. 校验 ORT DLL 的 SHA-256 和 size_bytes 非空
//! 3. 校验每个模型的 SHA-256 和 size_bytes 非空
//! 4. 警告 placeholder hash（不阻塞构建，但标记未就绪）
//! 5. 生成/更新 `resources/stt/paraformer-onnx/manifest.json`（供应链摘要）
//!
//! ## 供应链策略
//!
//! - **正式安装包不携带 ORT DLL 或 ONNX 模型**：按需下载，asset-lock.json
//!   锁定 URL + SHA-256 + size_bytes 确保不可变性
//! - **blink.exe 自身是 worker 宿主**：通过 `ManagedProcess` spawn
//!   `blink.exe` 作为 worker，使用二进制协议 v2 通信
//! - **隔离 self-test**：部署时调用 `blink.exe paraformer-selftest`，
//!   加载 staging DLL + 创建 Session + 最小推理 + 协议验证
//!
//! ## 与 funasr-worker 的区别
//!
//! | | funasr-worker | paraformer-worker |
//! |---|---|---|
//! | 协议 | NDJSON v1 | Binary v2 |
//! | 构建 | CMake + MSVC 编译 C++ | 无独立二进制 |
//! | 宿主 | 独立 .exe | blink.exe 子进程 |
//! | 资产 | exe + manifest.json | ORT DLL + 模型（按需下载）|

use std::path::PathBuf;

use sha2::{Digest, Sha256};

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p
}

/// STT asset-lock.json 路径。
fn asset_lock_path() -> PathBuf {
    workspace_root().join("resources/stt/paraformer-onnx/asset-lock.json")
}

/// STT manifest.json 路径（供应链摘要，构建产物）。
fn manifest_path() -> PathBuf {
    workspace_root().join("resources/stt/paraformer-onnx/manifest.json")
}

/// 供应链校验结果。
struct SupplyChainReport {
    ort_version: String,
    ort_files_count: usize,
    models_count: usize,
    has_placeholder: bool,
    warnings: Vec<String>,
}

/// 校验 STT asset-lock.json 的完整性和一致性。
fn verify_asset_lock() -> Result<SupplyChainReport, String> {
    let path = asset_lock_path();
    if !path.exists() {
        return Err(format!("STT asset-lock.json 不存在: {}", path.display()));
    }

    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("读取 asset-lock.json 失败: {e}"))?;

    let lock: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("asset-lock.json 不是合法 JSON: {e}"))?;

    let schema_version = lock
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .ok_or("asset-lock.json: schema_version 缺失")?;
    if schema_version != 1 {
        return Err(format!(
            "asset-lock.json: schema_version 不支持（期望 1，实际 {schema_version}）"
        ));
    }

    let ort_version = lock
        .get("ort")
        .and_then(|v| v.get("version"))
        .and_then(|v| v.as_str())
        .ok_or("asset-lock.json: ort.version 缺失")?
        .to_string();

    let ort_files = lock
        .get("ort")
        .and_then(|v| v.get("files"))
        .and_then(|v| v.as_array())
        .ok_or("asset-lock.json: ort.files 缺失或为空")?;

    if ort_files.is_empty() {
        return Err("asset-lock.json: ort.files 为空".to_string());
    }

    let models = lock
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or("asset-lock.json: models 缺失或为空")?;

    if models.is_empty() {
        return Err("asset-lock.json: models 为空".to_string());
    }

    let mut warnings = Vec::new();
    let mut has_placeholder = false;

    // 校验 ORT DLL 文件条目
    for file in ort_files {
        let path = file
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)");
        let sha = file.get("sha256").and_then(|v| v.as_str());
        let size = file.get("size_bytes").and_then(|v| v.as_u64());

        if sha.is_none() {
            return Err(format!("ORT file {path} 缺少 sha256"));
        }
        if size.is_none() {
            return Err(format!("ORT file {path} 缺少 size_bytes"));
        }
    }

    // 校验模型文件条目
    for model in models {
        let kind = model
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)");
        let filename = model
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)");
        let sha = model.get("sha256").and_then(|v| v.as_str());
        let size = model.get("size_bytes").and_then(|v| v.as_u64());
        let url = model.get("url").and_then(|v| v.as_str());

        if sha.is_none() {
            return Err(format!("模型 {kind} ({filename}) 缺少 sha256"));
        }
        if size.is_none() {
            return Err(format!("模型 {kind} ({filename}) 缺少 size_bytes"));
        }
        if url.is_none() {
            return Err(format!("模型 {kind} ({filename}) 缺少 url"));
        }

        // 检测 placeholder
        if let Some(s) = sha {
            if s.starts_with("placeholder-") {
                has_placeholder = true;
                warnings.push(format!(
                    "模型 {kind} ({filename}) 使用 placeholder hash——\
                     真实部署前需由供应链流水线填入实际 SHA-256"
                ));
            }
        }
        if let Some(0) = size {
            has_placeholder = true;
            warnings.push(format!(
                "模型 {kind} ({filename}) size_bytes=0——\
                 真实部署前需由供应链流水线填入实际大小"
            ));
        }

        // 检查 URL 浮动 ref（resolve/main/）
        if let Some(url) = url {
            if url.contains("/resolve/main/") {
                warnings.push(format!(
                    "模型 {kind} ({filename}) URL 使用浮动 ref (resolve/main)——\
                     建议替换为 /resolve/<commit-sha>/ 固定到不可变 revision"
                ));
            }
        }
    }

    // 校验必需的模型 kind 存在
    let kinds: Vec<&str> = models
        .iter()
        .filter_map(|m| m.get("kind").and_then(|v| v.as_str()))
        .collect();
    for required in ["encoder", "decoder", "cmvn", "tokenizer"] {
        if !kinds.contains(&required) {
            return Err(format!("asset-lock.json: 缺少必需的模型 kind: {required}"));
        }
    }

    Ok(SupplyChainReport {
        ort_version,
        ort_files_count: ort_files.len(),
        models_count: models.len(),
        has_placeholder,
        warnings,
    })
}

/// 生成供应链摘要 manifest.json。
///
/// 此文件不是运行时所需（asset-lock.json 才是运行时真源），
/// 但提供了快速可读的供应链摘要，便于 release-check 和调试。
fn generate_manifest(report: &SupplyChainReport) -> String {
    let mut hasher = Sha256::new();
    hasher.update(std::fs::read(asset_lock_path()).unwrap_or_default());
    let lock_hash = format!("{:x}", hasher.finalize());

    let manifest = serde_json::json!({
        "schema": 1,
        "description": "ParaformerOnline STT worker 供应链摘要",
        "asset_lock_sha256": lock_hash,
        "ort_version": report.ort_version,
        "ort_files_count": report.ort_files_count,
        "models_count": report.models_count,
        "has_placeholder_hashes": report.has_placeholder,
        "worker_protocol_version": 2,
        "worker_entry": "blink.exe paraformer-selftest",
        "notes": [
            "worker 不是独立二进制——blink.exe 自身是 worker 宿主",
            "正式安装包不携带 ORT DLL 或 ONNX 模型；按需下载",
            "资产完整性由 asset-lock.json 的 SHA-256 强校验保障",
            "隔离 self-test 通过 blink.exe paraformer-selftest 执行"
        ]
    });

    serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".to_string())
}

/// 构建入口：校验供应链并生成 manifest.json。
pub fn build_paraformer_worker() {
    println!("🔍 校验 ParaformerOnline STT worker 供应链...");

    let report = match verify_asset_lock() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("❌ STT asset-lock.json 校验失败: {e}");
            panic!("ParaformerOnline worker 供应链校验失败");
        }
    };

    println!("  ✓ asset-lock.json 校验通过");
    println!("    ORT version: {}", report.ort_version);
    println!("    ORT files: {}", report.ort_files_count);
    println!("    Models: {}", report.models_count);

    for warning in &report.warnings {
        println!("  ⚠ {warning}");
    }

    if report.has_placeholder {
        println!(
            "  ⚠ asset-lock.json 包含 placeholder hash——\
                   不可用于真实部署，仅限开发/测试"
        );
    }

    // 生成 manifest.json
    let manifest = generate_manifest(&report);
    let manifest_path = manifest_path();
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("创建 {} 失败: {e}", parent.display()));
    }
    std::fs::write(&manifest_path, &manifest)
        .unwrap_or_else(|e| panic!("写入 manifest.json 失败: {e}"));
    println!("  ✓ manifest.json 已生成: {}", manifest_path.display());

    println!("✅ ParaformerOnline worker 供应链校验完成");
}
