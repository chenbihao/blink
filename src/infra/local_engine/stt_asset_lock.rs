//! ParaformerOnline STT 供应链 asset lock（0.22.9）。
//!
//! 解析 `resources/stt/paraformer-onnx/asset-lock.json`，为
//! ParaformerOnline ONNX worker 提供版本化、不可变 artifact 的
//! URL、SHA-256、size 和 license 信息。
//!
//! ## 设计铁则
//!
//! - **编译期嵌入**：asset-lock.json 通过 `include_str!` 嵌入二进制，
//!   运行时零文件依赖。
//! - **SHA-256 强校验**：所有下载文件必须 hash 匹配才能 promote。
//! - **URL 固定 revision**：模型 URL 使用 SHA-256 强校验确保不可变性。
//! - **仅 CPU DLL**：只锁定 CPU-only ORT，不包含 CUDA/TensorRT provider。
//! - **与 OCR asset_lock 独立**：STT 和 OCR 可使用不同 ORT 版本，
//!   各自锁定，互不干扰。
//! - **占位 hash**：当前为 placeholder，真实部署前由供应链流水线填入
//!   实际 SHA-256 和 size_bytes。placeholder 不通过 validator 校验。

use serde::{Deserialize, Serialize};

use super::runtime::{ArtifactId, RuntimeError};

/// asset-lock.json 的编译期嵌入内容。
pub const ASSET_LOCK_JSON: &str =
    include_str!("../../../resources/stt/paraformer-onnx/asset-lock.json");

/// asset lock 根结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttAssetLock {
    /// schema 版本。
    pub schema_version: u32,
    /// ORT DLL 锁定信息（与 OCR 共享 ORT 版本但独立声明）。
    pub ort: OrtLock,
    /// 模型文件锁定列表（encoder / decoder / cmvn / tokenizer）。
    pub models: Vec<SttModelLock>,
}

/// ORT DLL 锁定信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrtLock {
    /// ORT 版本（如 `1.19.2`）。
    pub version: String,
    /// ORT git commit。
    #[allow(dead_code)]
    pub commit: String,
    /// ORT build type。
    #[allow(dead_code)]
    pub build_type: String,
    /// ORT archive 下载 URL。
    pub url: String,
    /// License。
    pub license: String,
    /// DLL 文件清单。
    pub files: Vec<LockedFile>,
}

/// STT 模型文件锁定信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttModelLock {
    /// 模型种类（encoder / decoder / cmvn / tokenizer）。
    pub kind: String,
    /// 模型显示名称。
    #[allow(dead_code)]
    pub name: String,
    /// 文件名。
    pub filename: String,
    /// 下载 URL。
    pub url: String,
    /// revision。
    #[allow(dead_code)]
    pub revision: String,
    /// SHA-256（hex）。
    pub sha256: String,
    /// 文件大小（字节）。
    pub size_bytes: u64,
    /// License。
    pub license: String,
}

/// 锁定文件条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedFile {
    /// archive 内相对路径。
    pub path: String,
    /// SHA-256（hex）。
    pub sha256: String,
    /// 文件大小（字节）。
    pub size_bytes: u64,
    /// 是否为 DLL。
    pub is_dll: bool,
}

/// 解析嵌入的 asset-lock.json。
pub fn parse_asset_lock() -> Result<SttAssetLock, RuntimeError> {
    serde_json::from_str(ASSET_LOCK_JSON).map_err(|e| RuntimeError::ManifestParseFailed {
        message: format!("STT asset-lock.json 解析失败: {e}"),
    })
}

/// 获取 ORT DLL artifact id（版本化、不可变）。
///
/// 与 OCR 共享同一 ORT 版本时可共享同一 artifact。
pub fn ort_dll_artifact_id() -> Result<ArtifactId, RuntimeError> {
    let lock = parse_asset_lock()?;
    ArtifactId::new(format!("ort-cpu-{}", lock.ort.version)).map_err(|e| {
        RuntimeError::ManifestParseFailed {
            message: format!("ORT artifact id 构造失败: {e}"),
        }
    })
}

/// 获取 encoder 模型锁定信息。
#[allow(dead_code)] // Handoff 07A: production wiring pending gate
pub fn encoder_model_lock() -> Result<SttModelLock, RuntimeError> {
    let lock = parse_asset_lock()?;
    lock.models
        .iter()
        .find(|m| m.kind == "encoder")
        .cloned()
        .ok_or_else(|| RuntimeError::ManifestParseFailed {
            message: "asset-lock.json 中缺少 encoder 模型".to_string(),
        })
}

/// 获取 decoder 模型锁定信息。
#[allow(dead_code)] // Handoff 07A: production wiring pending gate
pub fn decoder_model_lock() -> Result<SttModelLock, RuntimeError> {
    let lock = parse_asset_lock()?;
    lock.models
        .iter()
        .find(|m| m.kind == "decoder")
        .cloned()
        .ok_or_else(|| RuntimeError::ManifestParseFailed {
            message: "asset-lock.json 中缺少 decoder 模型".to_string(),
        })
}

/// 获取 CMVN 模型锁定信息。
#[allow(dead_code)] // Handoff 07A: production wiring pending gate
pub fn cmvn_lock() -> Result<SttModelLock, RuntimeError> {
    let lock = parse_asset_lock()?;
    lock.models
        .iter()
        .find(|m| m.kind == "cmvn")
        .cloned()
        .ok_or_else(|| RuntimeError::ManifestParseFailed {
            message: "asset-lock.json 中缺少 cmvn".to_string(),
        })
}

/// 获取 tokenizer 锁定信息。
#[allow(dead_code)] // Handoff 07A: production wiring pending gate
pub fn tokenizer_lock() -> Result<SttModelLock, RuntimeError> {
    let lock = parse_asset_lock()?;
    lock.models
        .iter()
        .find(|m| m.kind == "tokenizer")
        .cloned()
        .ok_or_else(|| RuntimeError::ManifestParseFailed {
            message: "asset-lock.json 中缺少 tokenizer".to_string(),
        })
}

/// 检查 asset lock 是否包含 placeholder hash（未填入真实 hash）。
///
/// 在 self-test 前调用：placeholder hash 的 asset lock 不可用于
/// 真实部署，只可用于架构占位。
pub fn has_placeholder_hashes() -> Result<bool, RuntimeError> {
    let lock = parse_asset_lock()?;
    for model in &lock.models {
        if model.sha256.starts_with("placeholder-") || model.size_bytes == 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_embedded_asset_lock() {
        let lock = parse_asset_lock().expect("asset-lock.json 解析成功");
        assert_eq!(lock.schema_version, 1);
        assert_eq!(lock.ort.version, "1.19.2");
        assert_eq!(lock.ort.files.len(), 2);
        assert_eq!(lock.models.len(), 4);
    }

    #[test]
    fn ort_dll_artifact_id_stable() {
        let id = ort_dll_artifact_id().expect("artifact id 构造成功");
        assert_eq!(id.as_str(), "ort-cpu-1.19.2");
    }

    #[test]
    fn encoder_model_exists() {
        let enc = encoder_model_lock().expect("encoder model lock");
        assert!(!enc.sha256.is_empty());
        assert_eq!(enc.filename, "encoder.onnx");
    }

    #[test]
    fn decoder_model_exists() {
        let dec = decoder_model_lock().expect("decoder model lock");
        assert!(!dec.sha256.is_empty());
        assert_eq!(dec.filename, "decoder.onnx");
    }

    #[test]
    fn cmvn_exists() {
        let cmvn = cmvn_lock().expect("cmvn lock");
        assert!(!cmvn.sha256.is_empty());
        assert_eq!(cmvn.filename, "am.mvn");
    }

    #[test]
    fn tokenizer_exists() {
        let tok = tokenizer_lock().expect("tokenizer lock");
        assert!(!tok.sha256.is_empty());
        assert_eq!(tok.filename, "tokenizer.json");
    }

    #[test]
    fn detects_placeholder_hashes() {
        // asset-lock.json 已填入真实 SHA-256 和 size_bytes
        assert!(!has_placeholder_hashes().unwrap());
    }

    #[test]
    fn model_kinds_are_complete() {
        let lock = parse_asset_lock().expect("asset lock");
        let kinds: Vec<&str> = lock.models.iter().map(|m| m.kind.as_str()).collect();
        assert!(kinds.contains(&"encoder"));
        assert!(kinds.contains(&"decoder"));
        assert!(kinds.contains(&"cmvn"));
        assert!(kinds.contains(&"tokenizer"));
    }
}
