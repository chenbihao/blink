//! FunASR GGUF 模型安装 worker（0.22.7）。
//!
//! 从锁定的 HuggingFace URL 下载 GGUF 文件到 staging payload 目录，
//! 逐文件流式 SHA-256 校验（spec 中的 hash 为编译期锁定值）。
//! 下载与校验通过后由 EngineManager 的模型资产事务负责 fingerprint/promote。
//!
//! **铁则**：
//! - 只接受编译期 `gguf_model_specs()` 中的 model id——不接受前端 URL；
//! - 下载写入 `.tmp_<name>` 临时名，hash 通过后原子改名；
//! - hash 不匹配 → 清理临时文件并返回 Failed（损坏修复走模型事务重装）；
//! - 取消/超时立即停止写入并清理；
//! - 不伪造下载百分比——按文件粒度报告阶段，按字节报告进度日志（节流）。
//! - 大文件 I/O（离线复制、hash 校验）通过 `spawn_blocking` 挪出 tokio executor，
//!   HTTP chunk 写入为 KB 级同步写，阻塞可忽略。

use std::path::Path;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::app::local_engine::model_installer::{
    InstallSink, ModelDownloadChecksumSource, ModelDownloadError, ModelDownloadOutcome,
    ModelInstallWorker,
};
use crate::infra::local_engine::runtime::EngineId;

use super::gguf::find_gguf_spec;

/// 单文件下载超时（GGUF 可达数百 MB，慢速网络需要长窗口）。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(3600);
/// 进度日志间隔（按已下载字节）。
const PROGRESS_LOG_STEP_BYTES: u64 = 32 * 1024 * 1024;

pub struct FunasrGgufModelInstallWorker;

impl FunasrGgufModelInstallWorker {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FunasrGgufModelInstallWorker {
    fn default() -> Self {
        Self::new()
    }
}

/// 日志回调类型（Send+Sync——download future 必须跨线程）。
type LogFn = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// 流式下载单文件到 `dest`（临时名 `.tmp_<name>`），校验 SHA-256 后改名。
///
/// 使用 `response.chunk()` 逐块拉取（无 futures Stream 依赖）；写盘为
/// 同步写（每块 KB 级，阻塞可忽略），hash 在内存中流式累计。
/// 取消/错误路径清理临时文件。
async fn download_file(
    client: &reqwest::Client,
    url: &str,
    expected_sha256: &str,
    dest_dir: &Path,
    file_name: &str,
    cancel_token: &CancellationToken,
    on_log: &LogFn,
) -> Result<(), ModelDownloadError> {
    use sha2::{Digest, Sha256};
    use std::io::Write;

    let tmp_path = dest_dir.join(format!(".tmp_{file_name}"));
    let final_path = dest_dir.join(file_name);

    on_log(&format!("下载 {file_name}（{url}）"));

    let mut response = tokio::select! {
        r = client.get(url).send() => r,
        _ = cancel_token.cancelled() => return Err(ModelDownloadError::Cancelled),
    }
    .map_err(|e| ModelDownloadError::Network {
        message: format!("请求失败: {e}"),
    })?;

    if !response.status().is_success() {
        return Err(ModelDownloadError::Network {
            message: format!("HTTP {} 下载 {file_name} 失败", response.status()),
        });
    }

    let mut file = std::fs::File::create(&tmp_path).map_err(|e| ModelDownloadError::Internal {
        message: format!("创建临时文件失败: {e}"),
    })?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    let mut next_progress = PROGRESS_LOG_STEP_BYTES;

    loop {
        let chunk = tokio::select! {
            c = response.chunk() => c,
            _ = cancel_token.cancelled() => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(ModelDownloadError::Cancelled);
            }
        };
        let chunk = chunk.map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            ModelDownloadError::Network {
                message: format!("下载中断: {e}"),
            }
        })?;
        let Some(bytes) = chunk else { break };

        hasher.update(&bytes);
        file.write_all(&bytes).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            ModelDownloadError::Internal {
                message: format!("写入 {file_name} 失败: {e}"),
            }
        })?;
        downloaded += bytes.len() as u64;
        if downloaded >= next_progress {
            on_log(&format!(
                "{file_name}: 已下载 {} MB",
                downloaded / (1024 * 1024)
            ));
            next_progress += PROGRESS_LOG_STEP_BYTES;
        }
    }
    file.flush().map_err(|e| ModelDownloadError::Internal {
        message: format!("flush {file_name} 失败: {e}"),
    })?;
    drop(file);

    let actual: String = {
        let digest = hasher.finalize();
        digest.iter().map(|b| format!("{b:02x}")).collect()
    };
    if actual != expected_sha256 {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(ModelDownloadError::Failed {
            message: format!(
                "{file_name} SHA-256 不匹配（期望 {expected_sha256}，实际 {actual}）——下载损坏或上游文件变更"
            ),
        });
    }

    // 原子改名到最终文件名
    std::fs::rename(&tmp_path, &final_path).map_err(|e| ModelDownloadError::Internal {
        message: format!("落盘 {file_name} 失败: {e}"),
    })?;
    on_log(&format!("{file_name} 校验通过（{downloaded} 字节）"));
    Ok(())
}

#[async_trait::async_trait]
impl ModelInstallWorker for FunasrGgufModelInstallWorker {
    async fn download_to_staging(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        revision: &str,
        staging_payload_dir: &Path,
        cancel_token: CancellationToken,
        sink: Option<std::sync::Arc<dyn InstallSink>>,
    ) -> Result<ModelDownloadOutcome, ModelDownloadError> {
        let log: LogFn = match &sink {
            Some(s) => {
                let s = s.clone();
                std::sync::Arc::new(move |line: &str| s.emit_log(line))
            }
            None => std::sync::Arc::new(|_line: &str| {}),
        };

        let spec = find_gguf_spec(model_id).ok_or_else(|| ModelDownloadError::Internal {
            message: format!("model_id '{model_id}' 不在 GGUF 模型目录中"),
        })?;
        if revision != spec.revision {
            return Err(ModelDownloadError::Internal {
                message: format!("revision 不匹配：期望 {}，请求 {}", spec.revision, revision),
            });
        }

        log(&format!(
            "开始安装 GGUF 模型 {model_id}（{} 个文件）",
            spec.files.len()
        ));
        if let Some(s) = &sink {
            s.emit_stage("downloading");
        }

        // 创建 staging 目录——同步操作但快速完成（mkdir 仅创建路径组件）
        let staging_dir = staging_payload_dir.to_path_buf();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&staging_dir))
            .await
            .map_err(|e| ModelDownloadError::Internal {
                message: format!("spawn_blocking create_dir_all 失败: {e}"),
            })?
            .map_err(|e| ModelDownloadError::Internal {
                message: format!("创建 staging 目录失败: {e}"),
            })?;

        let client = reqwest::Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            .build()
            .map_err(|e| ModelDownloadError::Internal {
                message: format!("HTTP client 创建失败: {e}"),
            })?;

        let _ = engine_id; // 引擎绑定由模型事务保证；worker 只按 spec 下载
        for file in &spec.files {
            // 取消检查：每个文件开始前
            if cancel_token.is_cancelled() {
                return Err(ModelDownloadError::Cancelled);
            }

            // 已存在且 hash 正确的文件跳过（断网/重装复用）
            let final_path = staging_payload_dir.join(file.file_name);
            if final_path.is_file() {
                let ok = tokio::task::spawn_blocking({
                    let p = final_path.clone();
                    let sha = file.sha256.clone();
                    let ct = cancel_token.clone();
                    move || -> Option<bool> {
                        use sha2::{Digest, Sha256};
                        use std::io::Read;
                        let mut f = std::fs::File::open(&p).ok()?;
                        let mut h = Sha256::new();
                        let mut buf = vec![0u8; 1024 * 1024];
                        loop {
                            if ct.is_cancelled() {
                                return None;
                            }
                            let n = f.read(&mut buf).ok()?;
                            if n == 0 {
                                break;
                            }
                            h.update(&buf[..n]);
                        }
                        let actual: String =
                            h.finalize().iter().map(|b| format!("{b:02x}")).collect();
                        Some(actual == sha)
                    }
                })
                .await
                .ok()
                .flatten()
                .unwrap_or(false);
                if ok {
                    log(&format!(
                        "{} 已存在且校验通过，跳过下载（离线复用）",
                        file.file_name
                    ));
                    continue;
                }
                // hash 不符的残留文件删除重下（损坏修复）
                let _ = std::fs::remove_file(&final_path);
            }

            // 离线缓存源（0.22.7.3）：BLINK_GGUF_MODEL_CACHE 指向预置模型目录时
            // 优先从本地复制（复制后仍做 SHA-256 校验）；未命中或校验失败回退网络。
            if let Some(cache_dir) = offline_cache_dir() {
                let cached = cache_dir.join(file.file_name);
                if cached.is_file() {
                    log(&format!(
                        "{} 命中离线缓存（{}），本地复制并校验",
                        file.file_name,
                        cached.display()
                    ));
                    match copy_and_verify(&cached, &final_path, &file.sha256, &cancel_token).await {
                        Ok(()) => continue,
                        Err(e) => {
                            if matches!(e, ModelDownloadError::Cancelled) {
                                return Err(e);
                            }
                            log(&format!("缓存校验失败（{e}），回退网络下载"));
                            let _ = std::fs::remove_file(&final_path);
                        }
                    }
                }
            }

            download_file(
                &client,
                &file.url,
                &file.sha256,
                staging_payload_dir,
                file.file_name,
                &cancel_token,
                &log,
            )
            .await?;
        }

        if let Some(s) = &sink {
            s.emit_stage("downloaded");
        }

        Ok(ModelDownloadOutcome {
            source: format!("huggingface:FunAudioLLM ({model_id})"),
            checksum_source: ModelDownloadChecksumSource::Sha256(
                spec.files
                    .iter()
                    .map(|f| f.sha256.clone())
                    .collect::<Vec<_>>()
                    .join("+"),
            ),
        })
    }
}

/// 离线缓存目录（`BLINK_GGUF_MODEL_CACHE` 环境变量，0.22.7.3）。
///
/// 预置模型目录（文件名与 spec 一致）优先本地复制 + SHA-256 校验；
/// 命中失败回退网络下载。用于离线部署与 E2E 测试预置。
fn offline_cache_dir() -> Option<std::path::PathBuf> {
    std::env::var("BLINK_GGUF_MODEL_CACHE")
        .ok()
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir())
}

/// 复制文件并校验 SHA-256（离线缓存路径）。
///
/// GGUF 文件可达数百 MB，整个复制 + hash 过程在 `spawn_blocking` 中执行，
/// 不阻塞 tokio executor。复制使用临时名，hash 通过后原子改名。
/// 取消检查在每个 1MB 块边界执行。
async fn copy_and_verify(
    src: &Path,
    dest: &Path,
    expected_sha256: &str,
    cancel_token: &CancellationToken,
) -> Result<(), ModelDownloadError> {
    let src = src.to_path_buf();
    let dest = dest.to_path_buf();
    let tmp = dest.with_extension("tmp_copy");
    let expected = expected_sha256.to_string();
    let ct = cancel_token.clone();

    tokio::task::spawn_blocking(move || -> Result<(), ModelDownloadError> {
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};

        let mut f = std::fs::File::open(&src).map_err(|e| ModelDownloadError::Internal {
            message: format!("打开缓存失败: {e}"),
        })?;
        let mut out = std::fs::File::create(&tmp).map_err(|e| ModelDownloadError::Internal {
            message: format!("写临时文件失败: {e}"),
        })?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            if ct.is_cancelled() {
                let _ = std::fs::remove_file(&tmp);
                return Err(ModelDownloadError::Cancelled);
            }
            let n = f.read(&mut buf).map_err(|e| ModelDownloadError::Internal {
                message: format!("读缓存失败: {e}"),
            })?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .map_err(|e| ModelDownloadError::Internal {
                    message: format!("写临时文件失败: {e}"),
                })?;
        }
        out.flush().map_err(|e| ModelDownloadError::Internal {
            message: format!("flush 失败: {e}"),
        })?;
        drop(out);

        let actual: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if actual != expected {
            let _ = std::fs::remove_file(&tmp);
            return Err(ModelDownloadError::Failed {
                message: format!("SHA-256 不匹配（期望 {expected}，实际 {actual}）"),
            });
        }
        std::fs::rename(&tmp, dest).map_err(|e| ModelDownloadError::Internal {
            message: format!("落盘失败: {e}"),
        })?;
        Ok(())
    })
    .await
    .map_err(|e| ModelDownloadError::Internal {
        message: format!("spawn_blocking copy_and_verify 失败: {e}"),
    })?
}
