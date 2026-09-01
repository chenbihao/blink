//! ONNX OCR 供应链 asset lock（0.22.8-B）。
//!
//! 解析 `resources/ocr/paddleocr-onnx/asset-lock.json`，为 OnnxRuntimeProvider
//! 提供版本化、不可变 artifact 的 URL、SHA-256、size 和 license 信息。
//!
//! ## 设计铁则
//!
//! - **编译期嵌入**：asset-lock.json 通过 `include_str!` 嵌入二进制，
//!   运行时零文件依赖。
//! - **SHA-256 强校验**：所有下载文件必须 hash 匹配才能 promote。
//! - **URL 固定 revision**：模型 URL 使用 SHA-256 强校验确保不可变性，
//!   即使 URL 包含 `resolve/main/`，hash 不匹配时拒绝 promote。
//! - **仅 CPU DLL**：只锁定 CPU-only ORT，不包含 CUDA/TensorRT provider。

use serde::{Deserialize, Serialize};

use super::runtime::{ArtifactId, RuntimeError};

/// asset-lock.json 的编译期嵌入内容。
pub const ASSET_LOCK_JSON: &str =
    include_str!("../../../resources/ocr/paddleocr-onnx/asset-lock.json");

/// asset lock 根结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetLock {
    /// schema 版本。
    pub schema_version: u32,
    /// ORT DLL 锁定信息。
    pub ort: OrtLock,
    /// 模型文件锁定列表（det / rec / dictionary）。
    pub models: Vec<ModelLock>,
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

/// 模型文件锁定信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelLock {
    /// 模型种类（det / rec / dictionary）。
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
pub fn parse_asset_lock() -> Result<AssetLock, RuntimeError> {
    serde_json::from_str(ASSET_LOCK_JSON).map_err(|e| RuntimeError::ManifestParseFailed {
        message: format!("asset-lock.json 解析失败: {e}"),
    })
}

/// 获取 ORT DLL artifact id（版本化、不可变）。
pub fn ort_dll_artifact_id() -> Result<ArtifactId, RuntimeError> {
    let lock = parse_asset_lock()?;
    ArtifactId::new(format!("ort-cpu-{}", lock.ort.version)).map_err(|e| {
        RuntimeError::ManifestParseFailed {
            message: format!("ORT artifact id 构造失败: {e}"),
        }
    })
}

/// 获取 det 模型锁定信息。
#[allow(dead_code)]
pub fn det_model_lock() -> Result<ModelLock, RuntimeError> {
    let lock = parse_asset_lock()?;
    lock.models
        .iter()
        .find(|m| m.kind == "det")
        .cloned()
        .ok_or_else(|| RuntimeError::ManifestParseFailed {
            message: "asset-lock.json 中缺少 det 模型".to_string(),
        })
}

/// 获取 rec 模型锁定信息。
#[allow(dead_code)]
pub fn rec_model_lock() -> Result<ModelLock, RuntimeError> {
    let lock = parse_asset_lock()?;
    lock.models
        .iter()
        .find(|m| m.kind == "rec")
        .cloned()
        .ok_or_else(|| RuntimeError::ManifestParseFailed {
            message: "asset-lock.json 中缺少 rec 模型".to_string(),
        })
}

/// 获取 dictionary 锁定信息。
#[allow(dead_code)]
pub fn dict_lock() -> Result<ModelLock, RuntimeError> {
    let lock = parse_asset_lock()?;
    lock.models
        .iter()
        .find(|m| m.kind == "dictionary")
        .cloned()
        .ok_or_else(|| RuntimeError::ManifestParseFailed {
            message: "asset-lock.json 中缺少 dictionary".to_string(),
        })
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
        assert_eq!(lock.models.len(), 3);
    }

    #[test]
    fn ort_dll_artifact_id_stable() {
        let id = ort_dll_artifact_id().expect("artifact id 构造成功");
        assert_eq!(id.as_str(), "ort-cpu-1.19.2");
    }

    #[test]
    fn det_model_has_sha256() {
        let det = det_model_lock().expect("det model lock");
        assert!(!det.sha256.is_empty());
        assert_eq!(det.size_bytes, 1780590);
    }

    #[test]
    fn rec_model_has_sha256() {
        let rec = rec_model_lock().expect("rec model lock");
        assert!(!rec.sha256.is_empty());
        assert_eq!(rec.size_bytes, 4462639);
    }

    #[test]
    fn dict_has_sha256() {
        let dict = dict_lock().expect("dict lock");
        assert!(!dict.sha256.is_empty());
        assert_eq!(dict.size_bytes, 27156);
    }

    #[test]
    fn ort_dll_sha256_matches_spike() {
        let lock = parse_asset_lock().expect("asset lock");
        let dll = lock
            .ort
            .files
            .iter()
            .find(|f| f.path.ends_with("onnxruntime.dll"))
            .expect("onnxruntime.dll entry");
        assert_eq!(
            dll.sha256,
            "14119125df2dcf9ff3e083afdba5fcc4b09b4186d8762404eb7b1fbccde3fcf2"
        );
        assert_eq!(dll.size_bytes, 11234848);
        assert!(dll.is_dll);
    }

    #[test]
    fn model_urls_have_sha256_lock() {
        // 模型 URL 可以使用 resolve/main/（HuggingFace 稳定 release 分支），
        // 但必须有 SHA-256 强校验确保不可变性。
        let lock = parse_asset_lock().expect("asset lock");
        for model in &lock.models {
            assert!(
                !model.sha256.is_empty(),
                "模型 {} 缺少 SHA-256 锁定",
                model.filename
            );
            assert!(
                model.size_bytes > 0,
                "模型 {} size_bytes 为 0",
                model.filename
            );
        }
    }
}
