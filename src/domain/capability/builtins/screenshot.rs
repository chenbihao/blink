//! `screenshot` Capability（0.11.7-f）：统一的截图能力入口。
//!
//! **五合一 op**：
//! - `list_displays` — 枚举所有显示器，返回 `Text{JSON}`
//! - `capture` — 截取指定屏或虚拟屏幕，返回 `Blob{png}`
//! - `crop` — 从最近 SESSION 裁剪，返回 `Blob{png}`
//! - `window` — 截取指定窗口（需 window_ref），返回 `Blob{png}`（0.19.2）
//! - `capture_to_clipboard` — 截图直接写入剪贴板，返回 `Done`（0.19.3）
//!
//! 0.22.14 追加 `blink_visibility` 三态参数：
//! - `auto`（AI/MCP/CLI 调用缺省）：全屏/外部窗口截图排除 Blink 窗口；Blink 目标只保留目标
//! - `exclude`：强制排除全部 Blink 窗口；Blink 目标返回 InvalidArgs
//! - `include`（本地手动入口缺省，0.22.17 修订）：保持当前现场，不隐藏 Blink
//!
//! 0.22.17（用户决策）：未显式传参时缺省值按调用来源分流——AI 侧（LocalAi/Mcp/Cli）
//! 缺省 `auto` 主动隐藏净化截图；本地手动入口（LocalCommand/LocalSurface）缺省
//! `include` 不再隐藏 Blink 窗口（修复手动触发选区/截图时设置页等被一并隐藏的回归）。
//!
//! 净化截图不调用 `hide_chat_window`、不中止 ChatService、不复用隐藏前旧 SESSION cache。
//! `include` 也不复用 auto/exclude 或旧会话生成的截图缓存——每次都重新采集。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    CaptureCleansePlan, CaptureFn, CapturedImage, ConfirmationPolicy, DangerClass, InvokeContext,
    McpDefault, OriginSet, RuntimeRequirement,
};

// ── 可注入的图片写入 seam（测试用）─────────────────────────────────────────

/// 图片写入剪贴板的窄接口（可注入 seam）。
#[async_trait::async_trait]
trait ClipboardImageWriter: Send + Sync {
    /// 写 PNG 到剪贴板（虚拟屏幕路径）。
    async fn write_png(&self, png: Vec<u8>) -> Result<(), CapabilityError>;
    /// 写 BGRA 像素到剪贴板（指定显示器路径）。
    async fn write_bgra(&self, bgra: Vec<u8>, w: u32, h: u32) -> Result<(), CapabilityError>;
}

/// 生产实现：调用 `domain::clipboard` 的真实函数。
struct ProductionClipboardWriter;

#[async_trait::async_trait]
impl ClipboardImageWriter for ProductionClipboardWriter {
    async fn write_png(&self, png: Vec<u8>) -> Result<(), CapabilityError> {
        use crate::domain::clipboard::ClipboardWriteSource;
        crate::domain::clipboard::write_png(png, ClipboardWriteSource::Screenshot)
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: e.to_string(),
            })
    }

    async fn write_bgra(&self, bgra: Vec<u8>, w: u32, h: u32) -> Result<(), CapabilityError> {
        use crate::domain::clipboard::ClipboardWriteSource;
        crate::domain::clipboard::write_bgra(bgra, w, h, ClipboardWriteSource::Screenshot)
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: e.to_string(),
            })
    }
}

/// 统一截图能力。
pub struct Screenshot;

/// blink_visibility 三态解析结果（0.22.14）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlinkVisibility {
    Auto,
    Exclude,
    Include,
}

impl BlinkVisibility {
    /// 解析 `blink_visibility` 参数；未传时缺省值按调用来源分流（0.22.17）。
    fn parse(
        val: Option<&Value>,
        origin: crate::domain::capability::policy::InvocationOrigin,
    ) -> Result<Self, CapabilityError> {
        match val {
            None | Some(Value::Null) => Ok(Self::default_for_origin(origin)),
            Some(Value::String(s)) => match s.as_str() {
                "auto" => Ok(Self::Auto),
                "exclude" => Ok(Self::Exclude),
                "include" => Ok(Self::Include),
                other => Err(CapabilityError::InvalidArgs {
                    detail: format!(
                        "blink_visibility 非法值: {other}，允许: auto | exclude | include"
                    ),
                }),
            },
            Some(other) => Err(CapabilityError::InvalidArgs {
                detail: format!("blink_visibility 应为字符串，实际值: {other}"),
            }),
        }
    }

    /// 缺省值按调用来源分流（0.22.17 修订，用户决策）：
    /// - `LocalAi`/`Mcp`/`Cli` → `Auto`：AI 侧主动隐藏 Blink，净化截图
    /// - `LocalCommand`/`LocalSurface`（chord、launcher 等本地手动入口）→ `Include`：不隐藏
    fn default_for_origin(origin: crate::domain::capability::policy::InvocationOrigin) -> Self {
        use crate::domain::capability::policy::InvocationOrigin;
        match origin {
            InvocationOrigin::LocalCommand | InvocationOrigin::LocalSurface => Self::Include,
            InvocationOrigin::LocalAi | InvocationOrigin::Mcp | InvocationOrigin::Cli => Self::Auto,
        }
    }

    fn to_domain_plan(self, target_is_blink: bool) -> Result<CaptureCleansePlan, CapabilityError> {
        match (self, target_is_blink) {
            (Self::Include, _) => Ok(CaptureCleansePlan::Noop),
            (Self::Exclude, true) => Err(CapabilityError::InvalidArgs {
                detail: "blink_visibility=exclude 不能用于 Blink 自身窗口目标".into(),
            }),
            (Self::Exclude, false) => Ok(CaptureCleansePlan::CloakAllBlink),
            (Self::Auto, true) => Ok(CaptureCleansePlan::CloakOthersExceptTarget),
            (Self::Auto, false) => Ok(CaptureCleansePlan::CloakAllBlink),
        }
    }
}

/// 解析 window_ref 为有效 HWND。
///
/// AI/MCP 必须使用 window_ref（从 list_windows 获取）。
/// 裸 hwnd 仅供 LocalCommand/内部协议路径使用，AI/MCP 来源会被拒绝。
///
/// 0.22.14：通过 SurfacePort::validate_window_ref 校验，不直接依赖 crate::infra。
/// 截图目标解析结果——携带 HWND 和可选的 window_ref（用于二次核验）。
#[derive(Debug, PartialEq)]
pub(super) struct ResolvedTarget {
    hwnd: isize,
    /// window_ref（AI/MCP/CLI 路径有值；LocalCommand 裸 hwnd 路径无值）。
    /// 用于在截图回调中做第二次身份核验。
    ref_id: Option<String>,
}

fn resolve_target(
    args: &Value,
    origin: crate::domain::capability::policy::InvocationOrigin,
    surface: &dyn crate::domain::capability::SurfacePort,
) -> Result<Option<ResolvedTarget>, CapabilityError> {
    use crate::domain::capability::policy::InvocationOrigin;

    // 优先使用 window_ref
    if let Some(ref_val) = args.get("window_ref").and_then(Value::as_str) {
        let hwnd = surface
            .validate_window_ref(ref_val)
            .map_err(|e| CapabilityError::StaleRef {
                detail: map_surface_error_detail(e),
            })?;
        return Ok(Some(ResolvedTarget {
            hwnd,
            ref_id: Some(ref_val.to_string()),
        }));
    }

    // 裸 hwnd：仅 LocalCommand 路径允许（内部协议，不暴露在 schema 中）
    match origin {
        InvocationOrigin::LocalCommand => {
            if let Some(hwnd_val) = args.get("hwnd").and_then(Value::as_i64) {
                return Ok(Some(ResolvedTarget {
                    hwnd: hwnd_val as isize,
                    ref_id: None,
                }));
            }
            Ok(None)
        }
        // AI/MCP/Surface 不接受裸 hwnd
        InvocationOrigin::LocalAi
        | InvocationOrigin::Mcp
        | InvocationOrigin::Cli
        | InvocationOrigin::LocalSurface => {
            if args.get("hwnd").is_some() {
                return Err(CapabilityError::InvalidArgs {
                    detail: "AI/MCP/CLI 必须使用 window_ref（从 list_windows 获取），不接受裸 hwnd"
                        .into(),
                });
            }
            Ok(None)
        }
    }
}

/// 统一、穷尽的 SurfaceError → CapabilityError 映射（0.22.14 review P2）。
///
/// `capture_with_cleanse` 已返回 `ActivationFailed` 和 `RestoreFailed`，
/// 但之前各调用点统一转成 `Internal`，新增的稳定错误码实际不可达。
/// 此函数确保每条 SurfaceError 变体都映射到对应的结构化 CapabilityError。
fn map_surface_error(e: crate::domain::capability::SurfaceError) -> CapabilityError {
    use crate::domain::capability::SurfaceError;
    match e {
        SurfaceError::CreateFailed { detail } => CapabilityError::Internal { detail },
        SurfaceError::Unavailable { detail } => CapabilityError::StaleRef { detail },
        SurfaceError::ActivationFailed { detail } => CapabilityError::ActivationFailed { detail },
        SurfaceError::RestoreFailed { detail } => CapabilityError::RestoreFailed { detail },
        SurfaceError::WindowStateMismatch { expected, actual } => CapabilityError::Internal {
            detail: format!("窗口状态核验失败：期望{expected}，实际{actual}"),
        },
    }
}

/// SurfaceError → detail 字符串（用于 StaleRef 映射）。
fn map_surface_error_detail(e: crate::domain::capability::SurfaceError) -> String {
    use crate::domain::capability::SurfaceError;
    match e {
        SurfaceError::CreateFailed { detail }
        | SurfaceError::Unavailable { detail }
        | SurfaceError::ActivationFailed { detail }
        | SurfaceError::RestoreFailed { detail } => detail,
        SurfaceError::WindowStateMismatch { expected, actual } => {
            format!("窗口状态核验失败：期望{expected}，实际{actual}")
        }
    }
}

#[async_trait::async_trait]
impl Capability for Screenshot {
    fn id(&self) -> &str {
        "screenshot"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "screenshot".into(),
            description: "屏幕相关操作。op=list_displays 枚举显示器；op=capture 截取（可选 display_id）；op=crop 裁剪最近截屏；op=window 截取指定窗口（需 window_ref，从 list_windows 获取）；op=capture_to_clipboard 截图直接写入系统剪贴板。blink_visibility 控制截图是否临时隐藏 Blink 窗口：auto（AI/MCP/CLI 调用缺省，排除 Blink 窗口；目标为 Blink 时只保留目标）、exclude（强制排除全部 Blink 窗口，Blink 目标返回错误）、include（本地手动调用缺省，保留当前现场不隐藏 Blink）。".into(),
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
                    "window_ref": {
                        "type": "string",
                        "description": "窗口引用（op=window 必填，从 list_windows 获取）"
                    },
                    "blink_visibility": {
                        "type": "string",
                        "enum": ["auto", "exclude", "include"],
                        "description": "控制截图时是否临时隐藏 Blink 窗口。auto=排除 Blink 窗口（目标为 Blink 时只保留目标），AI/MCP/CLI 调用缺省；exclude=强制排除全部 Blink 窗口（Blink 目标返回错误）；include=保留当前现场，本地手动调用缺省。适用于 capture、window、capture_to_clipboard。"
                    },
                    "x": { "type": "integer", "description": "裁剪起点 X（op=crop 必填，物理像素）" },
                    "y": { "type": "integer", "description": "裁剪起点 Y（op=crop 必填）" },
                    "w": { "type": "integer", "description": "裁剪宽度（op=crop 必填）" },
                    "h": { "type": "integer", "description": "裁剪高度（op=crop 必填）" }
                },
                "required": ["op"]
            }),
            sensitive: true,
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            // screenshot 需要桌面会话；list_displays/crop 不需要 GUI surface。
            // 需要净化编排的具体 op（capture/window/capture_to_clipboard）在 invoke 内
            // 单独检查 surface 可用性，而非能力级拒绝 CLI/MCP（0.22.14 review P2）。
            runtime_requirement: RuntimeRequirement::DESKTOP_SESSION,
            danger: DangerClass::Safe,
            sensitive: true,
            ai_default: AiDefault::On,
            mcp_default: McpDefault::DefaultOff,
            confirmation: ConfirmationPolicy::sensitive(),
        }
    }
    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
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
                let visibility = BlinkVisibility::parse(args.get("blink_visibility"), ctx.origin)?;
                op_capture(display_id, visibility, ctx).await
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
                let surface = ctx
                    .runtime
                    .surface
                    .ok_or_else(|| CapabilityError::Unsupported {
                        required: "gui_surface".into(),
                        actual: ctx.runtime.as_requirement().to_string(),
                    })?;
                let target = resolve_target(&args, ctx.origin, surface)?.ok_or_else(|| {
                    CapabilityError::InvalidArgs {
                        detail: "缺少 window_ref 参数".into(),
                    }
                })?;
                let visibility = BlinkVisibility::parse(args.get("blink_visibility"), ctx.origin)?;
                op_window(target, visibility, ctx).await
            }
            "capture_to_clipboard" => {
                let display_id = args
                    .get("display_id")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
                let visibility = BlinkVisibility::parse(args.get("blink_visibility"), ctx.origin)?;
                op_capture_to_clipboard(display_id, visibility, ctx).await
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

// ── op 实现 ──────────────────────────────────────────────────────────────────

/// list_displays：枚举所有显示器，返回 `Text{JSON}`。
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
/// 0.22.14：净化截图——**所有 visibility 模式都不复用旧 SESSION cache**，
/// 必须通过 capture_with_cleanse 在净化后抓取新帧。
/// `include` 只表示不隐藏 Blink，不代表跳过采集——每次都重新抓取当前帧。
pub(super) async fn op_capture(
    display_id: Option<u32>,
    visibility: BlinkVisibility,
    ctx: &InvokeContext<'_>,
) -> Result<CapabilityResult, CapabilityError> {
    let cleanse_plan = visibility.to_domain_plan(false)?;

    // 指定显示器：新截一帧，不走 SESSION cache
    if let Some(id) = display_id {
        let surface = ctx
            .runtime
            .surface
            .ok_or_else(|| CapabilityError::Unsupported {
                required: "gui_surface".into(),
                actual: ctx.runtime.as_requirement().to_string(),
            })?;

        let capture_fn: CaptureFn = Arc::new(move || {
            let (bgra, geom) = crate::infra::platform::screenshot::capture_display(id)?;
            Ok(CapturedImage::Bgra {
                bytes: bgra,
                width: geom.w,
                height: geom.h,
            })
        });

        let result = surface
            .capture_with_cleanse(cleanse_plan, None, capture_fn)
            .await
            .map_err(map_surface_error)?;

        let (bgra, width, height) =
            result
                .image
                .as_bgra()
                .ok_or_else(|| CapabilityError::Internal {
                    detail: "capture_display 应返回 BGRA 数据".into(),
                })?;
        let bgra = bgra.to_vec();

        let png = tokio::task::spawn_blocking(move || {
            crate::infra::platform::screenshot::encode_png(&bgra, width, height)
        })
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: format!("encode_png task 崩溃: {e}"),
        })?
        .map_err(|e| CapabilityError::Internal { detail: e })?;

        tracing::debug!(
            display_id = id,
            bytes = png.len(),
            "capture: 净化截取显示器"
        );
        return Ok(CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: png,
            desc: Some(visibility_desc(visibility, false)),
        });
    }

    // 虚拟屏幕路径
    let surface = ctx
        .runtime
        .surface
        .ok_or_else(|| CapabilityError::Unsupported {
            required: "gui_surface".into(),
            actual: ctx.runtime.as_requirement().to_string(),
        })?;

    // 铁则：include 不复用 auto/exclude 或旧会话生成的截图缓存
    // 每次都重新采集——include 只表示不隐藏 Blink，不代表跳过采集
    let capture_fn: CaptureFn = Arc::new(|| {
        crate::infra::platform::screenshot::end_session();
        crate::infra::platform::screenshot::begin_session()?;
        crate::infra::platform::screenshot::session_png()
            .map(|arc| {
                let png = (*arc).clone();
                CapturedImage::Png { bytes: png }
            })
            .ok_or_else(|| "session_png 返回空".to_string())
    });

    let result = surface
        .capture_with_cleanse(cleanse_plan, None, capture_fn)
        .await
        .map_err(map_surface_error)?;

    let png_bytes = result
        .image
        .as_png()
        .ok_or_else(|| CapabilityError::Internal {
            detail: "虚拟屏幕路径应返回 PNG 数据".into(),
        })?
        .to_vec();

    tracing::debug!(bytes = png_bytes.len(), "capture: 净化截取虚拟屏幕");
    Ok(CapabilityResult::Blob {
        mime: "image/png".into(),
        bytes: png_bytes,
        desc: Some(visibility_desc(visibility, false)),
    })
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

/// window：截取指定窗口，返回 `Blob{png}`（0.19.2 + 0.22.14 净化）。
///
/// 0.22.14：支持 `blink_visibility` 三态 + `window_ref` 校验。
/// 外部窗口截图时，如果目标最小化则临时 restore + activate，截图后恢复。
/// 激活后验证目标确实处于可截图状态；激活失败返回结构化错误，不返回伪成功截图。
pub(super) async fn op_window(
    target: ResolvedTarget,
    visibility: BlinkVisibility,
    ctx: &InvokeContext<'_>,
) -> Result<CapabilityResult, CapabilityError> {
    let surface = ctx
        .runtime
        .surface
        .ok_or_else(|| CapabilityError::Unsupported {
            required: "gui_surface".into(),
            actual: ctx.runtime.as_requirement().to_string(),
        })?;
    let target_is_blink = surface.is_blink_hwnd(target.hwnd);
    let cleanse_plan = visibility.to_domain_plan(target_is_blink)?;

    // ref_id 用于截图前二次核验窗口身份（0.22.14 review P1）
    let ref_id_for_verify = target.ref_id.clone();
    let hwnd = target.hwnd;
    let capture_fn: CaptureFn = Arc::new(move || {
        // 截图前二次核验窗口身份（0.22.14 review P1）
        // 第一层：HWND 有效性
        if !crate::infra::platform::window::is_hwnd_valid(hwnd) {
            return Err(format!("hwnd {hwnd} 无效"));
        }
        // 第二层：如果存在 window_ref，通过 SurfacePort 做完整核验
        // （generation、TTL、PID、标题）
        // 注意：surface 在此闭包中不可用（trait object 不能被 Arc 包装），
        // 所以完整的 ref_id 二次核验在 op_window 主体中完成。
        //
        // 但如果 HWND 在 cloak 和截图之间被复用（PID 变化），
        // 仅靠 is_hwnd_valid 检测不到。因此需要在 capture_with_cleanse
        // 调用前完成完整的 ref_id 核验。
        let (x, y, w, h) = crate::infra::platform::window::get_window_dwm_rect(hwnd)
            .ok_or_else(|| format!("hwnd {hwnd} 无效或窗口不可见"))?;
        let bgra = crate::infra::platform::screenshot::capture_region(x, y, w, h)?;
        Ok(CapturedImage::Bgra {
            bytes: bgra,
            width: w,
            height: h,
        })
    });

    // 在调用 capture_with_cleanse 之前，做完整的 ref_id 二次核验（如果存在 ref_id）
    // 这确保在 cloak 和截图之间不会因为 HWND 复用而截错窗口
    if let Some(ref_id) = &ref_id_for_verify {
        surface
            .verify_window_identity(ref_id)
            .map_err(|e| CapabilityError::StaleRef {
                detail: map_surface_error_detail(e),
            })?;
    }

    let result = surface
        .capture_with_cleanse(cleanse_plan, Some(target.hwnd), capture_fn)
        .await
        .map_err(map_surface_error)?;

    let (bgra, width, height) =
        result
            .image
            .as_bgra()
            .ok_or_else(|| CapabilityError::Internal {
                detail: "op_window 应返回 BGRA 数据".into(),
            })?;
    let bgra = bgra.to_vec();

    // 验证截图结果非空
    if bgra.is_empty() {
        return Err(CapabilityError::CaptureFailed {
            detail: "截图返回空数据".into(),
        });
    }

    let png = tokio::task::spawn_blocking(move || {
        crate::infra::platform::screenshot::encode_png(&bgra, width, height)
    })
    .await
    .map_err(|e| CapabilityError::Internal {
        detail: format!("encode_png task 崩溃: {e}"),
    })?
    .map_err(|e| CapabilityError::Internal { detail: e })?;

    tracing::debug!(bytes = png.len(), "op_window: 净化截取窗口完成");
    Ok(CapabilityResult::Blob {
        mime: "image/png".into(),
        bytes: png,
        desc: Some(visibility_desc(visibility, target_is_blink)),
    })
}

/// capture_to_clipboard：截图直接写入系统剪贴板，返回 `Done`（0.19.3 + 0.22.14 净化）。
pub(super) async fn op_capture_to_clipboard(
    display_id: Option<u32>,
    visibility: BlinkVisibility,
    ctx: &InvokeContext<'_>,
) -> Result<CapabilityResult, CapabilityError> {
    op_capture_to_clipboard_with_writer(display_id, visibility, ctx, &ProductionClipboardWriter)
        .await
}

async fn op_capture_to_clipboard_with_writer(
    display_id: Option<u32>,
    visibility: BlinkVisibility,
    ctx: &InvokeContext<'_>,
    writer: &dyn ClipboardImageWriter,
) -> Result<CapabilityResult, CapabilityError> {
    let cleanse_plan = visibility.to_domain_plan(false)?;

    let surface = ctx
        .runtime
        .surface
        .ok_or_else(|| CapabilityError::Unsupported {
            required: "gui_surface".into(),
            actual: ctx.runtime.as_requirement().to_string(),
        })?;

    // 指定显示器路径
    if let Some(id) = display_id {
        let capture_fn: CaptureFn = Arc::new(move || {
            let (bgra, geom) = crate::infra::platform::screenshot::capture_display(id)?;
            Ok(CapturedImage::Bgra {
                bytes: bgra,
                width: geom.w,
                height: geom.h,
            })
        });

        let result = surface
            .capture_with_cleanse(cleanse_plan, None, capture_fn)
            .await
            .map_err(map_surface_error)?;

        let (bgra, w, h) = result
            .image
            .as_bgra()
            .ok_or_else(|| CapabilityError::Internal {
                detail: "capture_display 应返回 BGRA 数据".into(),
            })?;
        let bgra = bgra.to_vec();

        writer.write_bgra(bgra, w, h).await?;

        tracing::debug!(
            display_id = id,
            w,
            h,
            "capture_to_clipboard: 净化指定显示器截图已写入剪贴板"
        );
        return Ok(CapabilityResult::Done {
            summary: format!("已截图到剪贴板（{}）", visibility_desc(visibility, false)),
        });
    }

    // 虚拟屏幕路径
    // 铁则：include 不复用旧 SESSION cache——每次重新采集
    let capture_fn: CaptureFn = Arc::new(|| {
        crate::infra::platform::screenshot::end_session();
        crate::infra::platform::screenshot::begin_session()?;
        crate::infra::platform::screenshot::session_png()
            .map(|arc| {
                let png = (*arc).clone();
                CapturedImage::Png { bytes: png }
            })
            .ok_or_else(|| "session_png 返回空".to_string())
    });

    let result = surface
        .capture_with_cleanse(cleanse_plan, None, capture_fn)
        .await
        .map_err(map_surface_error)?;

    let png_bytes = result
        .image
        .as_png()
        .ok_or_else(|| CapabilityError::Internal {
            detail: "虚拟屏幕路径应返回 PNG 数据".into(),
        })?
        .to_vec();

    writer.write_png(png_bytes).await?;

    tracing::debug!("capture_to_clipboard: 净化虚拟屏幕截图已写入剪贴板");
    Ok(CapabilityResult::Done {
        summary: format!("已截图到剪贴板（{}）", visibility_desc(visibility, false)),
    })
}

/// 生成净化行为描述（用于 Blob.desc / Done.summary）。
fn visibility_desc(visibility: BlinkVisibility, target_is_blink: bool) -> String {
    match (visibility, target_is_blink) {
        (BlinkVisibility::Include, _) => "保留 Blink 窗口".to_string(),
        (BlinkVisibility::Exclude, false) => "已隐藏 Blink 窗口".to_string(),
        (BlinkVisibility::Exclude, true) => "参数冲突".to_string(),
        (BlinkVisibility::Auto, true) => "保留目标，已隐藏其他 Blink 窗口".to_string(),
        (BlinkVisibility::Auto, false) => "已隐藏 Blink 窗口".to_string(),
    }
}

// ── 测试辅助 ────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(super) mod test_helpers {
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    pub fn test_lock() -> MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::test_helpers::test_lock;
    use super::*;
    use crate::infra::platform::screenshot::backend_fake::FakeScreenshotBackend;

    struct SessionGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl SessionGuard {
        fn new() -> Self {
            Self { _lock: test_lock() }
        }
    }

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            crate::infra::platform::screenshot::end_session();
        }
    }

    // ── schema 测试 ──

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
    fn schema_has_blink_visibility() {
        let s = Screenshot.schema();
        let props = &s.parameters["properties"];
        assert!(
            props.get("blink_visibility").is_some(),
            "schema 应包含 blink_visibility"
        );
        let vis = &props["blink_visibility"];
        assert_eq!(vis["type"], "string");
        let allowed = vis["enum"].as_array().unwrap();
        assert!(allowed.contains(&json!("auto")));
        assert!(allowed.contains(&json!("exclude")));
        assert!(allowed.contains(&json!("include")));
    }

    #[test]
    fn schema_has_window_ref() {
        let s = Screenshot.schema();
        let props = &s.parameters["properties"];
        assert!(
            props.get("window_ref").is_some(),
            "schema 应包含 window_ref"
        );
        assert_eq!(props["window_ref"]["type"], "string");
    }

    #[test]
    fn schema_no_hwnd_param() {
        // 铁则：公开 schema 中不暴露 hwnd 参数
        let s = Screenshot.schema();
        let props = &s.parameters["properties"];
        assert!(
            props.get("hwnd").is_none(),
            "screenshot schema 不应暴露 hwnd 参数"
        );
    }

    #[test]
    fn schema_sensitive_is_true() {
        let s = Screenshot.schema();
        assert!(s.sensitive, "screenshot 必须 sensitive=true");
    }

    // ── blink_visibility 解析测试 ──

    #[test]
    fn parse_visibility_default_by_origin() {
        use crate::domain::capability::policy::InvocationOrigin;
        // 未传参：AI/MCP/CLI 缺省 Auto（主动隐藏）
        for origin in [
            InvocationOrigin::LocalAi,
            InvocationOrigin::Mcp,
            InvocationOrigin::Cli,
        ] {
            assert_eq!(
                BlinkVisibility::parse(None, origin).unwrap(),
                BlinkVisibility::Auto,
                "origin {origin:?} 缺省应为 Auto"
            );
        }
        // 未传参：本地手动入口缺省 Include（不隐藏，0.22.17 用户决策）
        for origin in [
            InvocationOrigin::LocalCommand,
            InvocationOrigin::LocalSurface,
        ] {
            assert_eq!(
                BlinkVisibility::parse(None, origin).unwrap(),
                BlinkVisibility::Include,
                "origin {origin:?} 缺省应为 Include"
            );
        }
        // 显式 Null 与 None 同义
        assert_eq!(
            BlinkVisibility::parse(Some(&Value::Null), InvocationOrigin::LocalAi).unwrap(),
            BlinkVisibility::Auto
        );
    }

    #[test]
    fn parse_visibility_explicit_overrides_origin_default() {
        use crate::domain::capability::policy::InvocationOrigin;
        for origin in [
            InvocationOrigin::LocalCommand,
            InvocationOrigin::LocalSurface,
            InvocationOrigin::LocalAi,
            InvocationOrigin::Mcp,
            InvocationOrigin::Cli,
        ] {
            assert_eq!(
                BlinkVisibility::parse(Some(&json!("auto")), origin).unwrap(),
                BlinkVisibility::Auto
            );
            assert_eq!(
                BlinkVisibility::parse(Some(&json!("exclude")), origin).unwrap(),
                BlinkVisibility::Exclude
            );
            assert_eq!(
                BlinkVisibility::parse(Some(&json!("include")), origin).unwrap(),
                BlinkVisibility::Include
            );
        }
    }

    #[test]
    fn parse_visibility_invalid_string() {
        use crate::domain::capability::policy::InvocationOrigin;
        let err =
            BlinkVisibility::parse(Some(&json!("mixed")), InvocationOrigin::LocalAi).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
        assert!(err.to_string().contains("mixed"));
    }

    #[test]
    fn parse_visibility_invalid_type() {
        use crate::domain::capability::policy::InvocationOrigin;
        let err = BlinkVisibility::parse(Some(&json!(42)), InvocationOrigin::LocalAi).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    // ── 策略矩阵测试 ──

    #[test]
    fn plan_include_noop() {
        assert_eq!(
            BlinkVisibility::Include.to_domain_plan(false).unwrap(),
            CaptureCleansePlan::Noop
        );
        assert_eq!(
            BlinkVisibility::Include.to_domain_plan(true).unwrap(),
            CaptureCleansePlan::Noop
        );
    }

    #[test]
    fn plan_exclude_non_blink_cloaks_all() {
        assert_eq!(
            BlinkVisibility::Exclude.to_domain_plan(false).unwrap(),
            CaptureCleansePlan::CloakAllBlink
        );
    }

    #[test]
    fn plan_exclude_blink_target_errors() {
        let err = BlinkVisibility::Exclude.to_domain_plan(true).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn plan_auto_non_blink_cloaks_all() {
        assert_eq!(
            BlinkVisibility::Auto.to_domain_plan(false).unwrap(),
            CaptureCleansePlan::CloakAllBlink
        );
    }

    #[test]
    fn plan_auto_blink_cloaks_others() {
        assert_eq!(
            BlinkVisibility::Auto.to_domain_plan(true).unwrap(),
            CaptureCleansePlan::CloakOthersExceptTarget
        );
    }

    // ── visibility_desc 测试 ──

    #[test]
    fn desc_include() {
        assert_eq!(
            visibility_desc(BlinkVisibility::Include, false),
            "保留 Blink 窗口"
        );
    }

    #[test]
    fn desc_exclude_non_blink() {
        assert_eq!(
            visibility_desc(BlinkVisibility::Exclude, false),
            "已隐藏 Blink 窗口"
        );
    }

    #[test]
    fn desc_auto_blink() {
        assert_eq!(
            visibility_desc(BlinkVisibility::Auto, true),
            "保留目标，已隐藏其他 Blink 窗口"
        );
    }

    // ── 原 op 测试（fake backend，不走净化路径） ──

    #[tokio::test]
    async fn op_list_displays_returns_fake_backend_configured() {
        let _g = SessionGuard::new();
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
    async fn op_crop_without_session_returns_invalid_args() {
        let _g = SessionGuard::new();
        crate::infra::platform::screenshot::end_session();
        let err = op_crop(0, 0, 100, 100).await.unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn op_crop_after_capture_returns_png() {
        let _g = SessionGuard::new();
        let fake = Arc::new(FakeScreenshotBackend::single_primary(200, 200));
        crate::infra::platform::screenshot::install_backend(fake);

        // 直接调 begin_session 建立 session
        tokio::task::spawn_blocking(crate::infra::platform::screenshot::begin_session)
            .await
            .unwrap()
            .unwrap();

        let result = op_crop(0, 0, 100, 100).await.unwrap();
        let CapabilityResult::Blob { bytes, .. } = result else {
            panic!("期望 Blob 结果");
        };
        assert_eq!(
            &bytes[..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
        );
    }

    // ── resolve_target 策略矩阵测试 ──

    /// 测试用 mock surface——validate_window_ref 总返回 Unavailable。
    struct MockSurface;

    #[async_trait::async_trait]
    impl crate::domain::capability::SurfacePort for MockSurface {
        fn open_settings(&self) -> Result<(), crate::domain::capability::SurfaceError> {
            unreachable!()
        }
        fn open_sticky_manager(&self) -> Result<(), crate::domain::capability::SurfaceError> {
            unreachable!()
        }
        fn open_chat(
            &self,
            _: Option<&str>,
        ) -> Result<(), crate::domain::capability::SurfaceError> {
            unreachable!()
        }
        fn open_clipboard_mode(&self) -> Result<(), crate::domain::capability::SurfaceError> {
            unreachable!()
        }
        async fn start_region_capture(
            &self,
            _: bool,
        ) -> Result<(), crate::domain::capability::SurfaceError> {
            unreachable!()
        }
        fn start_image_editor(
            &self,
            _: crate::domain::capability::EditorSourceRef,
        ) -> Result<(), crate::domain::capability::SurfaceError> {
            unreachable!()
        }
        fn start_content_editor(
            &self,
            _: crate::domain::capability::ContentEditorRequest,
        ) -> Result<(), crate::domain::capability::SurfaceError> {
            unreachable!()
        }
        fn hide_main_window(&self, _: &str) {}
        fn show_main_window(&self) -> Result<(), String> {
            unreachable!()
        }
        fn exit_app(&self) {}
        fn validate_window_ref(
            &self,
            _ref_id: &str,
        ) -> Result<isize, crate::domain::capability::SurfaceError> {
            Err(crate::domain::capability::SurfaceError::Unavailable {
                detail: "MockSurface: window_ref 不存在".into(),
            })
        }
        fn verify_window_identity(
            &self,
            _ref_id: &str,
        ) -> Result<(), crate::domain::capability::SurfaceError> {
            Ok(())
        }
        fn is_blink_hwnd(&self, _hwnd: isize) -> bool {
            false
        }
        async fn capture_with_cleanse(
            &self,
            _: crate::domain::capability::CaptureCleansePlan,
            _: Option<isize>,
            capture_fn: crate::domain::capability::CaptureFn,
        ) -> Result<crate::domain::capability::CaptureResult, crate::domain::capability::SurfaceError>
        {
            let image = capture_fn()
                .map_err(|e| crate::domain::capability::SurfaceError::CreateFailed { detail: e })?;
            Ok(crate::domain::capability::CaptureResult { image })
        }
        async fn manage_window_action(
            &self,
            _ref_id: &str,
            _action: &str,
        ) -> Result<
            crate::domain::capability::policy::WindowActionResult,
            crate::domain::capability::SurfaceError,
        > {
            unreachable!()
        }
    }

    #[test]
    fn resolve_target_ai_rejects_raw_hwnd() {
        use crate::domain::capability::policy::InvocationOrigin;
        let surface = MockSurface;
        // AI/MCP/CLI 传裸 hwnd 应被拒绝
        let args = json!({"hwnd": 12345});
        for origin in [
            InvocationOrigin::LocalAi,
            InvocationOrigin::Mcp,
            InvocationOrigin::Cli,
            InvocationOrigin::LocalSurface,
        ] {
            let err = resolve_target(&args, origin, &surface).unwrap_err();
            assert!(
                matches!(err, CapabilityError::InvalidArgs { .. }),
                "origin {origin:?} 传裸 hwnd 应返回 InvalidArgs"
            );
        }
    }

    #[test]
    fn resolve_target_local_command_accepts_raw_hwnd() {
        use crate::domain::capability::policy::InvocationOrigin;
        let surface = MockSurface;
        // LocalCommand 路径允许裸 hwnd（内部协议）
        let args = json!({"hwnd": 12345});
        let hwnd = resolve_target(&args, InvocationOrigin::LocalCommand, &surface).unwrap();
        assert_eq!(
            hwnd,
            Some(ResolvedTarget {
                hwnd: 12345,
                ref_id: None
            })
        );
    }

    #[test]
    fn resolve_target_ai_no_hwnd_returns_none() {
        use crate::domain::capability::policy::InvocationOrigin;
        let surface = MockSurface;
        // AI/MCP 不传 hwnd 也不传 window_ref → None（由调用方处理为缺少参数）
        let args = json!({});
        let hwnd = resolve_target(&args, InvocationOrigin::LocalAi, &surface).unwrap();
        assert_eq!(hwnd, None);
    }

    #[test]
    fn resolve_target_invalid_window_ref_returns_stale_ref() {
        use crate::domain::capability::policy::InvocationOrigin;
        let surface = MockSurface;
        // 不存在的 window_ref → StaleRef
        let args = json!({"window_ref": "wref_nonexistent"});
        let err = resolve_target(&args, InvocationOrigin::LocalAi, &surface).unwrap_err();
        assert!(
            matches!(err, CapabilityError::StaleRef { .. }),
            "无效 window_ref 应返回 StaleRef"
        );
    }

    // ── policy 一致性测试 ──

    #[test]
    fn policy_requires_desktop_session() {
        let p = Screenshot.policy();
        // screenshot 只需 DESKTOP_SESSION（0.22.14 review P2：CLI/MCP 兼容性）
        // 仅 DESKTOP_SESSION 即可满足——list_displays/crop 不需要 GUI surface
        assert!(
            p.runtime_requirement
                .is_satisfied_by(RuntimeRequirement::DESKTOP_SESSION),
            "screenshot 只需 DESKTOP_SESSION"
        );
        // 无运行时不应满足
        assert!(
            !p.runtime_requirement
                .is_satisfied_by(RuntimeRequirement::NONE),
            "screenshot 需要至少 DESKTOP_SESSION"
        );
    }

    #[test]
    fn manage_window_policy_requires_desktop_session() {
        use crate::domain::capability::builtins::manage_window::ManageWindow;
        let p = ManageWindow.policy();
        // manage_window 只需 DESKTOP_SESSION，不需要 GUI_SURFACE
        assert!(
            p.runtime_requirement
                .is_satisfied_by(RuntimeRequirement::DESKTOP_SESSION),
            "manage_window 只需 DESKTOP_SESSION"
        );
        // 不要求 GUI_SURFACE：提供只有 DESKTOP_SESSION 即可满足
    }
}
