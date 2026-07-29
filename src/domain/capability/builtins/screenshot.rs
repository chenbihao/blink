//! `screenshot` Capability（0.11.7-f）：统一的截图能力入口。
//!
//! **三合一 op**：
//! - `list_displays` — 枚举所有显示器，返回 `Text{JSON}`
//! - `capture` — 截取指定屏或虚拟屏幕，返回 `Blob{png}`
//! - `crop` — 从最近 SESSION 裁剪，返回 `Blob{png}`
//!
//! **与旧 alias 的关系**：
//! - `capture_screen` → 委托到 `screenshot { op: capture }`
//! - `crop_image` → 委托到 `screenshot { op: crop, x/y/w/h }`
//!
//! 旧 tool 名 alias 保留 3 个月，避免 AI 提示词层缓存失效（详见 phases/0.11.7 §12.6）。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};

/// 统一截图能力。
pub struct Screenshot;

#[async_trait::async_trait]
impl Capability for Screenshot {
    fn id(&self) -> &str {
        "screenshot"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "screenshot".into(),
            description: "屏幕相关操作。op=list_displays 枚举显示器；op=capture 截取（可选 display_id）；op=crop 裁剪最近截屏。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["list_displays", "capture", "crop"],
                        "description": "操作类型"
                    },
                    "display_id": {
                        "type": "integer",
                        "description": "显示器 id（op=capture 时可选，缺省截取虚拟屏幕）"
                    },
                    "x": { "type": "integer", "description": "裁剪起点 X（op=crop 必填，物理像素）" },
                    "y": { "type": "integer", "description": "裁剪起点 Y（op=crop 必填）" },
                    "w": { "type": "integer", "description": "裁剪宽度（op=crop 必填）" },
                    "h": { "type": "integer", "description": "裁剪高度（op=crop 必填）" }
                },
                "required": ["op"]
            }),
            ..Default::default()
        }
    }

    async fn invoke(
        &self,
        args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let op =
            args.get("op")
                .and_then(Value::as_str)
                .ok_or_else(|| CapabilityError::InvalidArgs {
                    detail: "缺少 op 参数".into(),
                })?;

        match op {
            "list_displays" => op_list_displays().await,
            "capture" => {
                let display_id = args
                    .get("display_id")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
                op_capture(display_id).await
            }
            "crop" => {
                let x = args.get("x").and_then(Value::as_i64).ok_or_else(|| {
                    CapabilityError::InvalidArgs {
                        detail: "缺少 x".into(),
                    }
                })? as i32;
                let y = args.get("y").and_then(Value::as_i64).ok_or_else(|| {
                    CapabilityError::InvalidArgs {
                        detail: "缺少 y".into(),
                    }
                })? as i32;
                let w = args.get("w").and_then(Value::as_u64).ok_or_else(|| {
                    CapabilityError::InvalidArgs {
                        detail: "缺少 w".into(),
                    }
                })? as u32;
                let h = args.get("h").and_then(Value::as_u64).ok_or_else(|| {
                    CapabilityError::InvalidArgs {
                        detail: "缺少 h".into(),
                    }
                })? as u32;
                op_crop(x, y, w, h).await
            }
            other => Err(CapabilityError::InvalidArgs {
                detail: format!("未知 op: {other}"),
            }),
        }
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(Screenshot) as Arc<dyn Capability>,
});

// ── op 实现（也供 alias Capability 复用） ─────────────────────────────────

/// list_displays：枚举所有显示器，返回 `Text{JSON}`。
///
/// **不 pub**：调用方走 Capability invoke，不直接调。测试通过 `Screenshot` invoke。
pub(super) async fn op_list_displays() -> Result<CapabilityResult, CapabilityError> {
    let displays = crate::infra::platform::screenshot::list_displays();
    let json = serde_json::to_string(&displays).map_err(|e| CapabilityError::Internal {
        detail: format!("序列化 displays 失败: {e}"),
    })?;
    Ok(CapabilityResult::Text { content: json, desc: None })
}

/// capture：截取指定显示器或虚拟屏幕，返回 `Blob{png}`。
///
/// `display_id=None` → 虚拟屏幕（复用 SESSION 缓存策略，与 0.9.7 一致）。
/// `display_id=Some(x)` → 指定显示器（新截，不走 SESSION cache——SESSION 只缓存虚拟屏幕）。
pub(super) async fn op_capture(
    display_id: Option<u32>,
) -> Result<CapabilityResult, CapabilityError> {
    // 指定显示器：新截一帧，不走 SESSION cache
    if let Some(id) = display_id {
        let (bgra, geom) = tokio::task::spawn_blocking(move || {
            crate::infra::platform::screenshot::capture_display(id)
        })
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: format!("capture_display task 崩溃: {e}"),
        })?
        .map_err(|e| CapabilityError::Internal { detail: e })?;

        let png = tokio::task::spawn_blocking(move || {
            crate::infra::platform::screenshot::encode_png(&bgra, geom.w, geom.h)
        })
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: format!("encode_png task 崩溃: {e}"),
        })?
        .map_err(|e| CapabilityError::Internal { detail: e })?;

        tracing::debug!(display_id = id, bytes = png.len(), "capture: 新截显示器");
        return Ok(CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: png,
            desc: None,
        });
    }

    // 虚拟屏幕：复用 SESSION cache 策略（0.9.7 甲方案）
    // 标注模式活跃时不复用——标注中的截图可能包含半成品标注
    if crate::infra::platform::screenshot::is_annotation_active() {
        tracing::debug!("capture: 标注模式活跃，跳过 SESSION cache");
    } else if let Some(png) = crate::infra::platform::screenshot::session_png() {
        tracing::debug!(bytes = png.len(), "capture: 复用 SESSION cache");
        return Ok(CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: png,
            desc: None,
        });
    }

    // 无 SESSION 或标注模式 → 新截一帧
    tokio::task::spawn_blocking(crate::infra::platform::screenshot::begin_session)
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: format!("截屏 task 崩溃: {e}"),
        })?
        .map_err(|e| CapabilityError::Internal { detail: e })?;

    match crate::infra::platform::screenshot::session_png() {
        Some(png) => {
            tracing::debug!(bytes = png.len(), "capture: 新截虚拟屏幕 + 编码 PNG");
            Ok(CapabilityResult::Blob {
                mime: "image/png".into(),
                bytes: png,
                desc: None,
            })
        }
        None => Err(CapabilityError::Internal {
            detail: "begin_session 成功但 session_png 返回空".into(),
        }),
    }
}

/// crop：从最近 SESSION 裁剪，返回 `Blob{png}`。
pub(super) async fn op_crop(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> Result<CapabilityResult, CapabilityError> {
    let png = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CapabilityError> {
        let (bgra, cw, ch) =
            crate::infra::platform::screenshot::crop(x, y, w, h).ok_or_else(|| {
                CapabilityError::InvalidArgs {
                    detail: "截图会话为空或裁剪区域无效".into(),
                }
            })?;
        crate::infra::platform::screenshot::encode_png(&bgra, cw, ch)
            .map_err(|e| CapabilityError::Internal { detail: e })
    })
    .await
    .map_err(|e| CapabilityError::Internal {
        detail: format!("crop task 崩溃: {e}"),
    })??;

    Ok(CapabilityResult::Blob {
        mime: "image/png".into(),
        bytes: png,
        desc: None,
    })
}

// ── 测试辅助（其他 builtin 的测试也可能与全局 backend 竞争，共享同一把锁） ─────

#[cfg(test)]
pub(super) mod test_helpers {
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    /// 获取共享测试锁：所有会写全局 SCREENSHOT backend / SESSION 的测试都应该拿这把锁。
    pub fn test_lock() -> MutexGuard<'static, ()> {
        // poisoned 时仍取 guard（前一个 test panic 不影响本 test 语义）
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::test_helpers::test_lock;
    use super::*;
    use crate::infra::platform::screenshot::backend_fake::FakeScreenshotBackend;

    #[test]
    fn id_is_screenshot() {
        assert_eq!(Screenshot.id(), "screenshot");
    }

    #[test]
    fn schema_declares_three_ops() {
        let s = Screenshot.schema();
        let ops = s.parameters["properties"]["op"]["enum"].as_array().unwrap();
        assert_eq!(ops.len(), 3);
        assert!(ops.contains(&json!("list_displays")));
        assert!(ops.contains(&json!("capture")));
        assert!(ops.contains(&json!("crop")));
    }

    /// list_displays 通过 fake backend 返回预设显示器列表。
    #[tokio::test]
    async fn op_list_displays_returns_fake_backend_configured() {
        let _g = test_lock();
        let fake = Arc::new(
            FakeScreenshotBackend::builder()
                .display(0, 0, 0, 2560, 1440, true)
                .display(1, 2560, 0, 1920, 1080, false)
                .build(),
        );
        crate::infra::platform::screenshot::install_backend(fake);

        let result = op_list_displays().await.unwrap();
        let CapabilityResult::Text { content, .. } = result else {
            panic!("期望 Text 结果");
        };
        let list: Vec<serde_json::Value> = serde_json::from_str(&content).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["primary"], json!(true));
        assert_eq!(list[0]["w"], json!(2560));
        assert_eq!(list[1]["primary"], json!(false));
    }

    #[tokio::test]
    async fn op_capture_virtual_screen_returns_png_blob() {
        let _g = test_lock();
        let fake = Arc::new(FakeScreenshotBackend::single_primary(800, 600));
        crate::infra::platform::screenshot::install_backend(fake);
        crate::infra::platform::screenshot::end_session();

        let result = op_capture(None).await.unwrap();
        let CapabilityResult::Blob { mime, bytes, .. } = result else {
            panic!("期望 Blob 结果");
        };
        assert_eq!(mime, "image/png");
        assert_eq!(
            &bytes[..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
        );
    }

    #[tokio::test]
    async fn op_capture_specific_display_returns_png() {
        let _g = test_lock();
        let fake = Arc::new(
            FakeScreenshotBackend::builder()
                .display(0, 0, 0, 800, 600, true)
                .display(1, 800, 0, 400, 300, false)
                .fill_color(0xFF, 0x00, 0x00, 0xFF)
                .build(),
        );
        crate::infra::platform::screenshot::install_backend(fake);

        let result = op_capture(Some(1)).await.unwrap();
        let CapabilityResult::Blob { bytes, .. } = result else {
            panic!("期望 Blob 结果");
        };
        assert_eq!(
            &bytes[..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
        );
    }

    #[tokio::test]
    async fn op_crop_without_session_returns_invalid_args() {
        let _g = test_lock();
        crate::infra::platform::screenshot::end_session();
        let err = op_crop(0, 0, 100, 100).await.unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn op_crop_after_capture_returns_png() {
        let _g = test_lock();
        let fake = Arc::new(FakeScreenshotBackend::single_primary(200, 200));
        crate::infra::platform::screenshot::install_backend(fake);
        let _ = op_capture(None).await.unwrap();

        let result = op_crop(0, 0, 100, 100).await.unwrap();
        let CapabilityResult::Blob { bytes, .. } = result else {
            panic!("期望 Blob 结果");
        };
        assert_eq!(
            &bytes[..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
        );
    }
}
