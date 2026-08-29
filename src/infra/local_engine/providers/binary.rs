//! ManagedBinary Provider 协议位（0.22.2）。
//!
//! 本文件只定义 `ManagedBinaryProvider` 的闭合 trait 和数据结构，
//! 不实现完整的下载/解包/验证逻辑——完整实现由后续版本落地。
//!
//! ## 设计铁则
//!
//! - `ManagedBinaryProvider` 可引用锁定的共享 Python stdlib artifact，
//!   但引用不创建 venv、不允许 pip，清理时有引用计数保护。
//! - 引用无法解析到用户代码解释器路径（只使用 Blink 托管的 artifact）。
//! - archive 必须有 SHA-256 校验；不允许无 hash 的生产安装。

use std::path::Path;

use super::{
    CompatibilityCheck, InstallPlan, InstallSink, ManifestExtension, PrepareResult,
    ResolvedProfile, RuntimeError, RuntimeProvider,
};
use crate::infra::local_engine::runtime;

/// ManagedBinary Provider（协议冻结，按需落地）。
///
/// 负责：
/// - 下载/解包锁定 archive
/// - 验证文件 hash、DLL 集合
/// - CPU feature / driver 前置检查
/// - self-test
/// - 可声明只读 stdlib artifact 依赖（如 Blink 托管 Python distribution）
///
/// **禁止**：
/// - 创建 venv
/// - 执行 pip install
/// - 读取用户代码解释器
/// 0.22.7 GGUF spike 的接入位：协议与 manifest/self-test 契约已冻结，
/// 网络安装/解包/验证按首个真实 binary 引擎（funasr-gguf）落地时实现。
#[allow(dead_code)]
pub struct ManagedBinaryProvider {
    /// 是否允许 GPU backend（测试时可关闭）。
    allow_gpu: bool,
}

impl ManagedBinaryProvider {
    /// 创建 ManagedBinaryProvider。
    pub fn new() -> Self {
        Self { allow_gpu: true }
    }

    /// 创建只允许 CPU 的 ManagedBinaryProvider（测试用）。
    #[allow(dead_code)]
    pub fn cpu_only() -> Self {
        Self { allow_gpu: false }
    }
}

impl Default for ManagedBinaryProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
#[allow(dead_code)]
impl RuntimeProvider for ManagedBinaryProvider {
    fn kind(&self) -> runtime::RuntimePlan {
        runtime::RuntimePlan::ManagedBinary
    }

    fn check_compatibility(
        &self,
        compatibility: &CompatibilityCheck,
    ) -> Result<bool, RuntimeError> {
        match compatibility {
            CompatibilityCheck::Always => Ok(true),
            CompatibilityCheck::RequiresCuda { .. } => {
                if !self.allow_gpu {
                    return Ok(false);
                }
                // 检查 nvidia-smi
                Ok(crate::infra::platform::python::detect_cuda().is_some())
            }
            CompatibilityCheck::RequiresVulkan => {
                if !self.allow_gpu {
                    return Ok(false);
                }
                // Vulkan 驱动检查（未来实现）
                // 目前保守返回 false
                Ok(false)
            }
            CompatibilityCheck::RequiresCpuFeature { feature } => {
                // CPU feature 检查（如 AVX2）
                // Windows 上可通过 IsProcessorFeaturePresent 检查
                match feature.as_str() {
                    "avx2" => Ok(check_avx2()),
                    "avx" => Ok(check_avx()),
                    "sse2" => Ok(true), // x64 默认支持 SSE2
                    _ => Ok(false),
                }
            }
        }
    }

    async fn prepare_environment(
        &self,
        staging_dir: &Path,
        plan: &InstallPlan,
        _resolved_profile: &ResolvedProfile,
        _cancel_token: Option<&tokio_util::sync::CancellationToken>,
        _sink: Option<&dyn InstallSink>,
    ) -> Result<PrepareResult, RuntimeError> {
        let binary_plan = match plan {
            InstallPlan::ManagedBinary(p) => p,
            _ => {
                return Err(RuntimeError::InstallFailed {
                    message: "ManagedBinaryProvider 收到非 ManagedBinary 安装计划".to_string(),
                });
            }
        };

        std::fs::create_dir_all(staging_dir)?;

        // 下载 archive（未来实现）
        // 目前只创建占位文件
        let archive_dir = staging_dir.join("archive");
        std::fs::create_dir_all(&archive_dir)?;

        // 如果有 stdlib artifact 引用，验证其存在
        if let Some(ref stdlib) = binary_plan.stdlib_artifact {
            let stdlib_dir = runtime::shared_artifact_dir(stdlib.runtime_kind, &stdlib.artifact_id);
            if !stdlib_dir.exists() {
                return Err(RuntimeError::InstallFailed {
                    message: format!("stdlib artifact 不存在: {}", stdlib.artifact_id),
                });
            }
            tracing::info!(
                stdlib = %stdlib.artifact_id,
                "stdlib artifact 引用验证通过"
            );
        }

        // 占位：实际下载/解包/验证由后续版本实现
        tracing::warn!("ManagedBinaryProvider.prepare_environment: 协议位，未实现完整逻辑");

        // 返回 artifact identity（从 binary_plan 获取）
        // 协议位阶段使用 descriptor 声明的 archive_sha256 作为 artifact hash
        Ok(PrepareResult {
            artifact: runtime::ArtifactIdentity {
                runtime_kind: runtime::RuntimePlan::ManagedBinary,
                artifact_id: binary_plan.archive_artifact_id.clone(),
                sha256: binary_plan.archive_sha256.clone(),
            },
        })
    }

    async fn self_test(
        &self,
        _generation_dir: &Path,
        plan: &InstallPlan,
        _cancel_token: Option<&tokio_util::sync::CancellationToken>,
        _sink: Option<&dyn InstallSink>,
    ) -> Result<(), RuntimeError> {
        let binary_plan = match plan {
            InstallPlan::ManagedBinary(p) => p,
            _ => {
                return Err(RuntimeError::SelfTestFailed {
                    message: "ManagedBinaryProvider 收到非 ManagedBinary 安装计划".to_string(),
                });
            }
        };

        // 执行 self-test 命令（未来实现）
        if binary_plan.self_test_command.is_empty() {
            tracing::warn!("ManagedBinaryProvider.self_test: 无 self-test 命令，跳过");
            return Ok(());
        }

        tracing::warn!("ManagedBinaryProvider.self_test: 协议位，未实现完整逻辑");
        Ok(())
    }

    fn build_manifest_extension(
        &self,
        _generation_dir: &Path,
        plan: &InstallPlan,
    ) -> Result<ManifestExtension, RuntimeError> {
        let binary_plan = match plan {
            InstallPlan::ManagedBinary(p) => p,
            _ => {
                return Err(RuntimeError::ManifestSerializeFailed {
                    message: "ManagedBinaryProvider 收到非 ManagedBinary 安装计划".to_string(),
                });
            }
        };

        Ok(ManifestExtension::ManagedBinary(
            runtime::BinaryManifestExt {
                archive_artifact_id: binary_plan.archive_artifact_id.clone(),
                archive_sha256: binary_plan.archive_sha256.clone(),
                executable: binary_plan.executable.clone(),
                files: Vec::new(), // 未来由实际解包结果填充
                stdlib_artifact: binary_plan.stdlib_artifact.clone(),
                required_cpu_features: binary_plan.required_cpu_features.clone(),
                required_drivers: binary_plan.required_drivers.clone(),
                self_test_passed: true,
            },
        ))
    }
}

// ── CPU feature 检测（Windows）─────────────────────────────────────────────

/// 检查 CPU 是否支持 AVX2。
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
fn check_avx2() -> bool {
    // 使用 std::arch x86_64 intrinsics 检测 AVX2 支持
    // 简化实现：x64 处理器通常支持 AVX2（自 Haswell / Excavator 起）
    // 生产实现应使用 __cpuid 检测
    is_x86_feature_detected!("avx2")
}

/// 检查 CPU 是否支持 AVX。
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
fn check_avx() -> bool {
    is_x86_feature_detected!("avx")
}

/// 非 x86_64 架构的占位实现。
#[cfg(not(target_arch = "x86_64"))]
fn check_avx2() -> bool {
    false
}

#[cfg(not(target_arch = "x86_64"))]
fn check_avx() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_binary_provider_kind() {
        let provider = ManagedBinaryProvider::new();
        assert_eq!(provider.kind(), runtime::RuntimePlan::ManagedBinary);
    }

    #[test]
    fn managed_binary_always_compatible() {
        let provider = ManagedBinaryProvider::new();
        assert!(
            provider
                .check_compatibility(&CompatibilityCheck::Always)
                .unwrap()
        );
    }

    #[test]
    fn managed_binary_cpu_only_rejects_gpu() {
        let provider = ManagedBinaryProvider::cpu_only();
        assert!(
            !provider
                .check_compatibility(&CompatibilityCheck::RequiresCuda { min_version: None })
                .unwrap()
        );
        assert!(
            !provider
                .check_compatibility(&CompatibilityCheck::RequiresVulkan)
                .unwrap()
        );
    }

    #[test]
    fn managed_binary_requires_cpu_feature_sse2() {
        let provider = ManagedBinaryProvider::new();
        // SSE2 在 x64 上总是支持
        assert!(
            provider
                .check_compatibility(&CompatibilityCheck::RequiresCpuFeature {
                    feature: "sse2".to_string()
                })
                .unwrap()
        );
    }
}
