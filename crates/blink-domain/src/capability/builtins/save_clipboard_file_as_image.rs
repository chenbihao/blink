//! 「存为图片条目」——把单文件 PNG/JPEG/静态 WebP 读取/解码后按现有图片历史
//! 路径入库（转 PNG + 256 缩略图 + sha256 去重 + `max_image_items` 容量清理）。
//!
//! 以用户点击触发校验（打开菜单时不读取文件）；读取/解码与预览共用服务
//! （受限路径防御 + 有界文件 I/O + 共享解码额度）。`source_path` 保存原路径
//! 中的文件名；文件条目自身保留完整路径；入库不修改系统剪贴板。
//! 提交前复核历史引用仍存在；失败/取消不留半成品。

use crate::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: String,
}

pub struct SaveClipboardFileAsImage;
#[async_trait::async_trait]
impl Capability for SaveClipboardFileAsImage {
    fn id(&self) -> &str {
        "save_clipboard_file_as_image"
    }
    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: self.id().into(),
            description: "Save a single PNG/JPEG/static WebP file entry into PNG image history (GIF is preview-only)"
                .into(),
            parameters: json!({"type":"object","properties":{
                "id":{"type":"string","description":"clipboard_files 条目 id（须恰好单路径）"}},
                "required":["id"],"additionalProperties":false}),
            ..Default::default()
        }
    }
    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::LOCAL_SURFACE | OriginSet::LOCAL_COMMAND,
            runtime_requirement: RuntimeRequirement::GUI_SURFACE,
            danger: DangerClass::Safe,
            sensitive: true,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::safe(),
        }
    }
    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let request: Request =
            serde_json::from_value(args).map_err(|e| CapabilityError::InvalidArgs {
                detail: e.to_string(),
            })?;
        let pools = ctx.env.db_pools();

        // 读取 + 解码（与预览共用服务：受限路径防御 + 有界文件 I/O + 共享解码额度）。
        // 保留原图尺寸；PNG 与 256px 缩略图在共享解码额度内一并生成。
        let decoded =
            blink_infra::data::clipboard_files::read_file_image_by_id(&pools.history, &request.id)
                .await
                .map_err(|e| CapabilityError::InvalidState {
                    detail: match e {
                        blink_infra::data::clipboard_files::FilePreviewError::EntryMissing => {
                            "文件条目不存在或非单文件".into()
                        }
                        blink_infra::data::clipboard_files::FilePreviewError::Busy => {
                            "文件预览繁忙，请稍后再试".into()
                        }
                        blink_infra::data::clipboard_files::FilePreviewError::Timeout => {
                            "文件读取超时，请稍后再试".into()
                        }
                        blink_infra::data::clipboard_files::FilePreviewError::Denied(reason) => {
                            format!("受限来源（{}），仅保存路径，未读取内容", reason.reason())
                        }
                        blink_infra::data::clipboard_files::FilePreviewError::NotImage => {
                            "不是支持的图片文件（PNG、JPEG、WebP）".into()
                        }
                        blink_infra::data::clipboard_files::FilePreviewError::PreviewOnly => {
                            "GIF 或动画图片仅支持预览，不能存为图片条目".into()
                        }
                        blink_infra::data::clipboard_files::FilePreviewError::PixelBudget => {
                            "图片像素数量超过转存上限".into()
                        }
                        blink_infra::data::clipboard_files::FilePreviewError::AnimationBudget => {
                            "动画帧数或累计像素超过预览上限".into()
                        }
                        blink_infra::data::clipboard_files::FilePreviewError::TooLarge => {
                            "文件超过 32MiB 上限".into()
                        }
                        blink_infra::data::clipboard_files::FilePreviewError::Decode(_) => {
                            "图片解码失败".into()
                        }
                    },
                })?;

        // 原路径文件名（source_path 语义与图片历史一致：仅文件名）
        let item = blink_infra::data::clipboard_files::get_paths_by_id(&pools.history, &request.id)
            .await
            .ok_or_else(|| CapabilityError::InvalidState {
                detail: "文件条目不存在或已删除".into(),
            })?;
        if item.paths.len() != 1 {
            return Err(CapabilityError::InvalidState {
                detail: "仅支持单文件入库".into(),
            });
        }
        let source_path =
            blink_infra::platform::clipboard::dropfiles::first_file_name(&item.paths[0])
                .map(str::to_string);

        let thumb = decoded
            .thumbnail_png
            .ok_or_else(|| CapabilityError::InvalidState {
                detail: "图片入库缺少缩略图".into(),
            })?;

        // 原图可达 32MiB，哈希也放阻塞线程，不在异步调度线程处理大缓冲。
        let png = decoded.png;
        let (png, sha256) = tokio::task::spawn_blocking(move || {
            let digest = Sha256::digest(&png);
            let sha256: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            (png, sha256)
        })
        .await
        .map_err(|_| CapabilityError::InvalidState {
            detail: "图片指纹计算失败".into(),
        })?;

        let image = blink_infra::data::clipboard_images::ClipboardImage {
            id: blink_infra::data::clipboard_images::generate_image_id(),
            sha256,
            png_blob: png,
            thumb_blob: thumb,
            width: decoded.width,
            height: decoded.height,
            created_at: chrono::Utc::now().timestamp(),
            source_app: Some("blink:file-import".into()),
            source_path,
        };
        // 提交前复核历史引用仍存在（读取/解码耗时期间条目可能被删）
        if blink_infra::data::clipboard_files::get_paths_by_id(&pools.history, &request.id)
            .await
            .is_none()
        {
            return Err(CapabilityError::InvalidState {
                detail: "文件条目已删除，入库中止".into(),
            });
        }
        blink_infra::data::clipboard_images::save_image(&pools.cache, &image)
            .await
            .map_err(|e| CapabilityError::InvalidState {
                detail: format!("图片入库失败: {e}"),
            })?;
        // 容量清理沿用图片上限配置（get 失败时用默认值兜底，不影响入库）
        let max_image_items = crate::config::ConfigStore::get::<
            blink_infra::data::clipboard::ClipboardConfig,
        >(&pools.config)
        .await
        .max_image_items;
        blink_infra::data::clipboard_images::cleanup_excess_images(&pools.cache, max_image_items)
            .await;
        tracing::info!(
            image_id = %image.id,
            w = image.width,
            h = image.height,
            "文件图片已入库（保留原文件条目）"
        );
        Ok(CapabilityResult::Done {
            summary: format!("已存为图片条目（{}x{}）", image.width, image.height),
        })
    }
}
inventory::submit!(crate::capability::CapabilityEntry {
    factory: || Arc::new(SaveClipboardFileAsImage) as Arc<dyn Capability>
});

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_is_local_sensitive_and_registered() {
        let registry = crate::capability::CapabilityRegistry::new();
        assert!(registry.get("save_clipboard_file_as_image").is_some());
        let policy = SaveClipboardFileAsImage.policy();
        assert!(policy.sensitive, "读取本地文件内容属敏感操作");
        assert!(
            !policy
                .allowed_origins
                .contains(crate::capability::InvocationOrigin::LocalAi)
        );
    }
}
