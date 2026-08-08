//! `screenshot` Capability（0.11.7-f）：统一的截图能力入口。
//!
//! **五合一 op**：
//! - `list_displays` — 枚举所有显示器，返回 `Text{JSON}`
//! - `capture` — 截取指定屏或虚拟屏幕，返回 `Blob{png}`
//! - `crop` — 从最近 SESSION 裁剪，返回 `Blob{png}`
//! - `window` — 截取指定窗口（按 hwnd），返回 `Blob{png}`（0.19.3）
//! - `capture_to_clipboard` — 截图直接写入剪贴板，返回 `Done`（0.19.6）
//!
//! 0.19.0 已删除 `capture_screen` / `crop_image` alias，统一走 `screenshot { op }`。

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
            description: "屏幕相关操作。op=list_displays 枚举显示器；op=capture 截取（可选 display_id）；op=crop 裁剪最近截屏；op=window 截取指定窗口（需 hwnd，从 list_windows 获取）；op=capture_to_clipboard 截图直接写入系统剪贴板（不返回图片数据）。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["list_displays", "capture", "crop", "window", "capture_to_clipboard"],
                        "description": "操作类型"
                    },
                    "display_id": {
                        "type": "integer",
                        "description": "显示器 id（op=capture 时可选，缺省截取虚拟屏幕）"
                    },
                    "hwnd": {
                        "type": "integer",
                        "description": "窗口句柄（op=window 必填，从 list_windows 获取）"
                    },
                    "x": { "type": "integer", "description": "裁剪起点 X（op=crop 必填，物理像素）" },
                    "y": { "type": "integer", "description": "裁剪起点 Y（op=crop 必填）" },
                    "w": { "type": "integer", "description": "裁剪宽度（op=crop 必填）" },
                    "h": { "type": "integer", "description": "裁剪高度（op=crop 必填）" }
                },
                "required": ["op"]
            }),
            sensitive: true, // 截图获取用户屏幕内容属隐私敏感数据（0.19.4 补齐）
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
            "window" => {
                let hwnd = args.get("hwnd").and_then(Value::as_i64).ok_or_else(|| {
                    CapabilityError::InvalidArgs {
                        detail: "缺少 hwnd 参数".into(),
                    }
                })? as isize;
                op_window(hwnd).await
            }
            "capture_to_clipboard" => {
                let display_id = args
                    .get("display_id")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
                op_capture_to_clipboard(display_id).await
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
    Ok(CapabilityResult::Text {
        content: json,
        desc: None,
    })
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

/// window：截取指定窗口，返回 `Blob{png}`（0.19.3）。
///
/// 入参 `hwnd`：从 `list_windows` Capability 拿到的窗口句柄（isize）。
///
/// **不依赖 SESSION cache**——与 `op:capture`（指定显示器）同理，每次新截。
/// 实现流程：`spawn_blocking` → `get_window_dwm_rect(hwnd)` 取 DWM 扩展边框
/// → `capture_region(x, y, w, h)` 截取虚拟屏幕对应区域 → `encode_png`。
///
/// **坐标系**：DWM rect 是虚拟屏幕物理像素坐标，与 `capture_region` 一致，
/// 无需转换。`get_window_dwm_rect` 返回的是 `DWMWA_EXTENDED_FRAME_BOUNDS`
/// （真实可视边框，非含阴影的 `GetWindowRect`），截图区域与用户所见窗口一致。
pub(super) async fn op_window(hwnd: isize) -> Result<CapabilityResult, CapabilityError> {
    let (bgra, w, h) =
        tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, u32, u32), CapabilityError> {
            let (x, y, w, h) =
                crate::infra::platform::window::get_window_dwm_rect(hwnd).ok_or_else(|| {
                    CapabilityError::InvalidArgs {
                        detail: format!("hwnd {hwnd} 无效或窗口不可见"),
                    }
                })?;
            let bgra = crate::infra::platform::screenshot::capture_region(x, y, w, h)
                .map_err(|e| CapabilityError::Internal { detail: e })?;
            Ok((bgra, w, h))
        })
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: format!("op_window task 崩溃: {e}"),
        })??;

    let png = tokio::task::spawn_blocking(move || {
        crate::infra::platform::screenshot::encode_png(&bgra, w, h)
    })
    .await
    .map_err(|e| CapabilityError::Internal {
        detail: format!("encode_png task 崩溃: {e}"),
    })?
    .map_err(|e| CapabilityError::Internal { detail: e })?;

    tracing::debug!(hwnd, bytes = png.len(), "op_window: 截取窗口完成");
    Ok(CapabilityResult::Blob {
        mime: "image/png".into(),
        bytes: png,
        desc: None,
    })
}

/// capture_to_clipboard：截图直接写入系统剪贴板，返回 `Done`（0.19.6）。
///
/// **目的**（roadmap §7.3 复合操作）：AI 只下指令收文本确认，MB 级图片不经过
/// LLM channel。与 `op:capture` 不同，本 op 不返回 `Blob`，而是将截图写入
/// 系统剪贴板（CF_DIB），AI 只收到 `Done{summary}`。
///
/// **复用 SESSION cache**：与 `op:capture`（虚拟屏幕路径）一致，标注模式活跃时
/// 跳过 cache，否则优先复用 SESSION 中已编码的 PNG。
///
/// **剪贴板写入**：
/// - 指定显示器路径：`capture_display` → BGRA → `write_bgra_to_clipboard`（零编码）
/// - 虚拟屏幕路径：`session_png` → PNG → `write_png_to_clipboard`（解码回 BGRA）
///
/// **自写入标记**：label=`blink:screenshot`，`skip_persist=false`（新截图应入库）。
pub(super) async fn op_capture_to_clipboard(
    display_id: Option<u32>,
) -> Result<CapabilityResult, CapabilityError> {
    use crate::infra::platform::clipboard::{SELF_LABEL_SCREENSHOT};

    // ── 指定显示器：capture_display → BGRA → write_bgra_to_clipboard（零编码）──
    if let Some(id) = display_id {
        let (bgra, geom) =
            tokio::task::spawn_blocking(move || crate::infra::platform::screenshot::capture_display(id))
                .await
                .map_err(|e| CapabilityError::Internal {
                    detail: format!("capture_display task 崩溃: {e}"),
                })?
                .map_err(|e| CapabilityError::Internal { detail: e })?;

        let (w, h) = (geom.w, geom.h);
        tokio::task::spawn_blocking(move || {
            crate::infra::platform::clipboard::write_bgra_to_clipboard(
                &bgra,
                w,
                h,
                SELF_LABEL_SCREENSHOT,
                false,
            )
        })
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: format!("write_bgra_to_clipboard task 崩溃: {e}"),
        })?
        .map_err(|e| CapabilityError::Internal { detail: e })?;

        tracing::debug!(display_id = id, w, h, "capture_to_clipboard: 指定显示器截图已写入剪贴板");
        return Ok(CapabilityResult::Done {
            summary: "已截图到剪贴板".into(),
        });
    }

    // ── 虚拟屏幕：复用 SESSION cache 策略（与 op:capture 一致）──────────────
    // 标注模式活跃时不复用——标注中的截图可能包含半成品标注
    let session_png = if crate::infra::platform::screenshot::is_annotation_active() {
        tracing::debug!("capture_to_clipboard: 标注模式活跃，跳过 SESSION cache");
        None
    } else {
        crate::infra::platform::screenshot::session_png()
    };

    let png = match session_png {
        Some(png) => {
            tracing::debug!(bytes = png.len(), "capture_to_clipboard: 复用 SESSION cache");
            png
        }
        None => {
            // 无 SESSION 或标注模式 → 新截一帧
            tokio::task::spawn_blocking(
                crate::infra::platform::screenshot::begin_session,
            )
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: format!("begin_session task 崩溃: {e}"),
            })?
            .map_err(|e| CapabilityError::Internal { detail: e })?;

            crate::infra::platform::screenshot::session_png().ok_or_else(|| {
                CapabilityError::Internal {
                    detail: "begin_session 成功但 session_png 返回空".into(),
                }
            })?
        }
    };

    // 写入剪贴板（PNG → 解码为 BGRA → CF_DIB）
    tokio::task::spawn_blocking(move || {
        crate::infra::platform::clipboard::write_png_to_clipboard(
            &png,
            SELF_LABEL_SCREENSHOT,
            false,
        )
    })
    .await
    .map_err(|e| CapabilityError::Internal {
        detail: format!("write_png_to_clipboard task 崩溃: {e}"),
    })?
    .map_err(|e| CapabilityError::Internal { detail: e })?;

    tracing::debug!("capture_to_clipboard: 虚拟屏幕截图已写入剪贴板");
    Ok(CapabilityResult::Done {
        summary: "已截图到剪贴板".into(),
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
    fn schema_declares_five_ops() {
        let s = Screenshot.schema();
        let ops = s.parameters["properties"]["op"]["enum"].as_array().unwrap();
        assert_eq!(ops.len(), 5);
        assert!(ops.contains(&json!("list_displays")));
        assert!(ops.contains(&json!("capture")));
        assert!(ops.contains(&json!("crop")));
        assert!(ops.contains(&json!("window")));
        assert!(ops.contains(&json!("capture_to_clipboard")));
    }

    #[test]
    fn schema_has_hwnd_param() {
        let s = Screenshot.schema();
        let props = &s.parameters["properties"];
        assert!(props.get("hwnd").is_some(), "schema 应包含 hwnd 参数");
        assert_eq!(props["hwnd"]["type"], "integer");
    }

    #[test]
    fn schema_sensitive_is_true() {
        let s = Screenshot.schema();
        assert!(
            s.sensitive,
            "screenshot 必须 sensitive=true（截取用户屏幕内容属隐私数据）"
        );
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

    /// hwnd=0 是 NULL HWND，`get_window_dwm_rect` 应返回 None → InvalidArgs。
    #[tokio::test]
    async fn op_window_invalid_hwnd_returns_error() {
        let _g = test_lock();
        let fake = Arc::new(FakeScreenshotBackend::single_primary(800, 600));
        crate::infra::platform::screenshot::install_backend(fake);

        let err = op_window(0).await.unwrap_err();
        assert!(
            matches!(err, CapabilityError::InvalidArgs { .. }),
            "期望 InvalidArgs，实际: {err:?}"
        );
    }

    /// 尝试用真实桌面窗口验证 op_window 全链路。
    ///
    /// `enumerate_pickable_windows()` 枚举桌面可见窗口，取第一个的 hwnd 调 `op_window`。
    /// 测试环境无可见窗口时 skip（不应 fail）。
    #[tokio::test]
    async fn op_window_with_real_window_returns_png() {
        let _g = test_lock();
        let fake = Arc::new(FakeScreenshotBackend::single_primary(1920, 1080));
        crate::infra::platform::screenshot::install_backend(fake);

        let windows = crate::infra::platform::window::enumerate_pickable_windows();
        let Some(win) = windows.first() else {
            // 测试环境无可见窗口——skip 而非 fail
            eprintln!("op_window_with_real_window_returns_png: 跳过（无可见窗口）");
            return;
        };

        let result = op_window(win.hwnd).await.unwrap();
        let CapabilityResult::Blob { mime, bytes, .. } = result else {
            panic!("期望 Blob 结果");
        };
        assert_eq!(mime, "image/png");
        assert_eq!(
            &bytes[..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
        );
    }

    /// capture_to_clipboard 虚拟屏幕路径：返回 Done，不返回 Blob。
    #[tokio::test]
    async fn op_capture_to_clipboard_virtual_screen_returns_done() {
        let _g = test_lock();
        let fake = Arc::new(FakeScreenshotBackend::single_primary(800, 600));
        crate::infra::platform::screenshot::install_backend(fake);
        crate::infra::platform::screenshot::end_session();

        let result = op_capture_to_clipboard(None).await.unwrap();
        let CapabilityResult::Done { summary } = result else {
            panic!("期望 Done 结果，实际: {result:?}");
        };
        assert!(summary.contains("剪贴板"), "summary 应提及剪贴板");
    }

    /// capture_to_clipboard 指定显示器路径：返回 Done。
    #[tokio::test]
    async fn op_capture_to_clipboard_specific_display_returns_done() {
        let _g = test_lock();
        let fake = Arc::new(
            FakeScreenshotBackend::builder()
                .display(0, 0, 0, 800, 600, true)
                .display(1, 800, 0, 400, 300, false)
                .build(),
        );
        crate::infra::platform::screenshot::install_backend(fake);

        let result = op_capture_to_clipboard(Some(1)).await.unwrap();
        let CapabilityResult::Done { summary } = result else {
            panic!("期望 Done 结果，实际: {result:?}");
        };
        assert!(summary.contains("剪贴板"), "summary 应提及剪贴板");
    }
}
