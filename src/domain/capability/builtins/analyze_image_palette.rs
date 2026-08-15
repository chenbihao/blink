//! `analyze_image_palette` Capability（0.20.7 修订）。
//!
//! 分析图片的配色方案，返回角色色、主题分析、推荐配色与设计取色。
//!
//! **配色核心单一真源**：所有色彩计算在 `domain::palette` 中完成，
//! 本 Capability 只负责输入解析（`image_ref` / `png`）和输出封装（`Items`）。
//!
//! **输入模式**（二选一）：
//! - `image_ref`：从 `ImageStash` 获取 PNG 字节，解码为 RGBA
//! - `png`：PNG 字节数组（有界兼容/测试输入），解码为 RGBA
//!
//! **资源上限**（常量集中定义）：
//! - 输入字节数 ≤ `MAX_INPUT_BYTES`（32 MiB）
//! - 解码后像素总数 ≤ `MAX_DECODED_PIXELS`（256 MiB / 4 bytes per pixel）
//!
//! **deadline**：保留 `ctx.is_expired()` 前置检查；
//! 分析是确定时限的纯 CPU 循环（k-means 聚类 + 角色分配 + 方案生成），
//! 无分段检查必要——若整个分析超过 deadline，调用方的 `invoke` 超时机制会兜底。
//!
//! **结果**：`CapabilityResult::Items`，每个方案一项 `ItemResult`，
//! `data` 为结构化 JSON（颜色项含 hex、rgb、占比、角色、对比度、推荐文字色；
//! 方案含稳定 id、label、description、source_kind、confidence）。
//! 中文展示文案不进 `data`——scheme 用稳定英文 id，label 由前端 i18n 负责。

use std::sync::Arc;

use serde_json::{Value, json};

use super::image_input::resolve_image_ref;
use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext, ItemResult,
};
use crate::domain::palette;

// ── 资源上限常量 ───────────────────────────────────────────────────────────

/// 输入 PNG 字节数上限（32 MiB）。
/// 参照 Image Editor 既有预算 `compressed/input bytes <= 32 MiB`。
const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;

/// 解码后 RGBA 像素总数上限（256 MiB / 4 bytes per pixel = 67_108_864 像素）。
/// 参照 Image Editor 既有预算 `decoded RGBA bytes <= 256 MiB`。
const MAX_DECODED_PIXELS: usize = (256 * 1024 * 1024) / 4;

/// `analyze_image_palette` — 分析图片配色，返回角色色、主题与推荐方案。
pub struct AnalyzeImagePalette;

#[async_trait::async_trait]
impl Capability for AnalyzeImagePalette {
    fn id(&self) -> &str {
        "analyze_image_palette"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "analyze_image_palette".into(),
            description: "分析图片的配色方案，返回角色色（背景/点缀/前景/弱化）、主题分析（色系/冷暖/饱和度/明度）、推荐配色方案和设计取色。图片来源：image_ref（来自截图/剪贴板等能力返回的引用）或 png（PNG 字节数组），二选一。可选 crop 参数指定裁剪区域。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "image_ref": {
                        "type": "string",
                        "description": "图片引用（来自 read_clipboard/screenshot 等能力返回的 image_ref，与 png 二选一）"
                    },
                    "png": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "PNG 字节数组（与 image_ref 二选一，有界兼容/测试输入）"
                    },
                    "crop": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer", "description": "裁剪起点 X（像素）" },
                            "y": { "type": "integer", "description": "裁剪起点 Y（像素）" },
                            "w": { "type": "integer", "description": "裁剪宽度（像素）" },
                            "h": { "type": "integer", "description": "裁剪高度（像素）" }
                        },
                        "description": "可选裁剪区域（像素坐标），在分析前裁剪"
                    }
                }
            }),
            sensitive: true, // 分析用户屏幕/剪贴板图片内容属隐私敏感数据
        }
    }

    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // deadline 前置检查
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "analyze_image_palette 截止时刻已过".into(),
            });
        }

        let (rgba_flat, width, height) = resolve_image_input(&args, ctx).await?;

        // 可选裁剪
        let (rgba_cropped, crop_w, crop_h) = if let Some(crop) = args.get("crop") {
            apply_crop(&rgba_flat, width, height, crop)?
        } else {
            (rgba_flat, width, height)
        };

        // CPU 密集：配色分析在 spawn_blocking 中执行
        // 分析是确定时限的纯 CPU 循环（k-means 聚类 + 角色分配 + 方案生成），
        // 无分段检查必要——调用方的 invoke 超时机制会兜底。
        let result = tokio::task::spawn_blocking(move || {
            palette::analyze_palette(&rgba_cropped, crop_w, crop_h)
        })
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: format!("配色分析 task 崩溃: {e}"),
        })?;

        tracing::debug!(
            width = crop_w,
            height = crop_h,
            roles = result.roles.len(),
            empty = result.empty,
            "analyze_image_palette: 分析完成"
        );

        // P0-3：返回 Items，每个方案一项 ItemResult
        let items = build_palette_items(&result);

        Ok(CapabilityResult::Items { items })
    }
}

/// 构建 PaletteResult → Vec<ItemResult>。
///
/// 每个方案（图片主题色/视觉焦点色/均衡配色/原图智能搭配/各生成方案）一项 ItemResult，
/// `data` 为结构化 JSON（不含中文展示文案/DOM 概念）。
/// `desc` 为可选副标题（给主窗口展示用），不含展示布局。
fn build_palette_items(result: &palette::PaletteResult) -> Vec<ItemResult> {
    if result.empty {
        return vec![];
    }

    let mut items = Vec::new();

    // 角色色 → 每个角色一项 ItemResult
    for role in &result.roles {
        let contrast = palette::recommend_text_color(role.rgb);
        items.push(ItemResult {
            data: json!({
                "type": "role_color",
                "rgb": [role.rgb[0], role.rgb[1], role.rgb[2]],
                "hex": role.hex,
                "role": role.role,
                "ratio": role.ratio,
                "contrast_ratio": contrast.ratio,
                "recommended_text_color": contrast.text_color,
            }),
            desc: Some(format!("{} · {}", role.hex, role.role)),
            actions: vec![],
        });
    }

    // 推荐方案 → 每个方案一项 ItemResult
    // P1-2：source_kind 和 confidence 从 HarmonyScheme 字段投影，不再硬编码
    for scheme in &result.recommended {
        let colors_data: Vec<Value> = scheme
            .colors
            .iter()
            .map(|rgb| {
                let hex = palette::rgb_to_hex(rgb[0], rgb[1], rgb[2]);
                let contrast = palette::recommend_text_color(*rgb);
                json!({
                    "rgb": [rgb[0], rgb[1], rgb[2]],
                    "hex": hex,
                    "contrast_ratio": contrast.ratio,
                    "recommended_text_color": contrast.text_color,
                })
            })
            .collect();

        items.push(ItemResult {
            data: json!({
                "type": "scheme",
                "scheme_id": scheme.scheme,
                "source_kind": scheme.source_kind,
                "confidence": scheme.confidence,
                "colors": colors_data,
                "description": scheme.description,
            }),
            desc: Some(scheme.label.clone()),
            actions: vec![],
        });
    }

    // 生成方案（full） → 每个方案一项 ItemResult
    // P1-2：source_kind 和 confidence 从 HarmonyScheme 字段投影
    for scheme in &result.full {
        let colors_data: Vec<Value> = scheme
            .colors
            .iter()
            .map(|rgb| {
                let hex = palette::rgb_to_hex(rgb[0], rgb[1], rgb[2]);
                let contrast = palette::recommend_text_color(*rgb);
                json!({
                    "rgb": [rgb[0], rgb[1], rgb[2]],
                    "hex": hex,
                    "contrast_ratio": contrast.ratio,
                    "recommended_text_color": contrast.text_color,
                })
            })
            .collect();

        items.push(ItemResult {
            data: json!({
                "type": "scheme",
                "scheme_id": scheme.scheme,
                "source_kind": scheme.source_kind,
                "confidence": scheme.confidence,
                "colors": colors_data,
                "description": scheme.description,
            }),
            desc: Some(scheme.label.clone()),
            actions: vec![],
        });
    }

    items
}

/// 解析图片输入：`image_ref` 或 `png` 二选一。
///
/// 返回 `(rgba_flat, width, height)`。
async fn resolve_image_input(
    args: &Value,
    ctx: &InvokeContext<'_>,
) -> Result<(Vec<u8>, usize, usize), CapabilityError> {
    let has_ref = args.get("image_ref").is_some();
    let has_png = args.get("png").is_some();

    if has_ref && has_png {
        return Err(CapabilityError::InvalidArgs {
            detail: "image_ref 和 png 不能同时提供，请二选一".into(),
        });
    }
    if !has_ref && !has_png {
        return Err(CapabilityError::InvalidArgs {
            detail: "必须提供 image_ref 或 png 之一".into(),
        });
    }

    if has_ref {
        // 从 ImageStash 获取 PNG 字节 → 解码为 RGBA
        let stash = ctx.env.image_stash();
        let png_bytes = resolve_image_ref(args, stash.map(|s| s.as_ref()))?;

        // 资源上限校验：输入字节数
        if png_bytes.len() > MAX_INPUT_BYTES {
            return Err(CapabilityError::InvalidData {
                reason: "input_too_large".into(),
                detail: format!(
                    "输入 PNG 字节数 {} 超过上限 {}",
                    png_bytes.len(),
                    MAX_INPUT_BYTES
                ),
            });
        }

        let (rgba, w, h) = tokio::task::spawn_blocking(move || {
            crate::infra::platform::screenshot::decode_png_to_rgba(&png_bytes)
        })
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: format!("PNG 解码 task 崩溃: {e}"),
        })?
        .map_err(|e| CapabilityError::InvalidData {
            reason: "png_decode".into(),
            detail: e,
        })?;

        // 资源上限校验：解码后像素总数
        let pixel_count = (w as usize) * (h as usize);
        if pixel_count > MAX_DECODED_PIXELS {
            return Err(CapabilityError::InvalidData {
                reason: "decoded_too_large".into(),
                detail: format!(
                    "解码后像素总数 {} ({w}x{h}) 超过上限 {}",
                    pixel_count, MAX_DECODED_PIXELS
                ),
            });
        }

        Ok((rgba, w as usize, h as usize))
    } else {
        // 直接使用 png 字节数组
        let png_bytes = super::image_input::parse_byte_array(args, "png")?;
        if png_bytes.is_empty() {
            return Err(CapabilityError::InvalidData {
                reason: "empty_input".into(),
                detail: "png 数据为空".into(),
            });
        }

        // 资源上限校验：输入字节数
        if png_bytes.len() > MAX_INPUT_BYTES {
            return Err(CapabilityError::InvalidData {
                reason: "input_too_large".into(),
                detail: format!(
                    "输入 PNG 字节数 {} 超过上限 {}",
                    png_bytes.len(),
                    MAX_INPUT_BYTES
                ),
            });
        }

        let (rgba, w, h) = tokio::task::spawn_blocking(move || {
            crate::infra::platform::screenshot::decode_png_to_rgba(&png_bytes)
        })
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: format!("PNG 解码 task 崩溃: {e}"),
        })?
        .map_err(|e| CapabilityError::InvalidData {
            reason: "png_decode".into(),
            detail: e,
        })?;

        // 资源上限校验：解码后像素总数
        let pixel_count = (w as usize) * (h as usize);
        if pixel_count > MAX_DECODED_PIXELS {
            return Err(CapabilityError::InvalidData {
                reason: "decoded_too_large".into(),
                detail: format!(
                    "解码后像素总数 {} ({w}x{h}) 超过上限 {}",
                    pixel_count, MAX_DECODED_PIXELS
                ),
            });
        }

        Ok((rgba, w as usize, h as usize))
    }
}

/// 对 RGBA flat 数据执行裁剪。
///
/// `crop` 为 JSON 对象 `{ x, y, w, h }`（像素坐标）。
/// 坐标越界会被 clamp 到有效范围。
fn apply_crop(
    rgba_flat: &[u8],
    width: usize,
    height: usize,
    crop: &Value,
) -> Result<(Vec<u8>, usize, usize), CapabilityError> {
    let x = crop
        .get("x")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as usize;
    let y = crop
        .get("y")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as usize;
    let w = crop
        .get("w")
        .and_then(Value::as_u64)
        .ok_or_else(|| CapabilityError::InvalidArgs {
            detail: "crop.w 缺失或非正整数".into(),
        })? as usize;
    let h = crop
        .get("h")
        .and_then(Value::as_u64)
        .ok_or_else(|| CapabilityError::InvalidArgs {
            detail: "crop.h 缺失或非正整数".into(),
        })? as usize;

    if w == 0 || h == 0 {
        return Err(CapabilityError::InvalidArgs {
            detail: "crop.w 和 crop.h 必须为正整数".into(),
        });
    }

    // clamp 到有效范围
    let x = x.min(width);
    let y = y.min(height);
    let crop_w = w.min(width.saturating_sub(x));
    let crop_h = h.min(height.saturating_sub(y));

    if crop_w == 0 || crop_h == 0 {
        return Err(CapabilityError::InvalidArgs {
            detail: "裁剪区域为空（x/y 超出图片范围）".into(),
        });
    }

    let stride = width * 4;
    let crop_stride = crop_w * 4;
    let mut result = Vec::with_capacity(crop_stride * crop_h);

    for row in y..(y + crop_h) {
        let start = row * stride + x * 4;
        let end = start + crop_stride;
        result.extend_from_slice(&rgba_flat[start..end]);
    }

    Ok((result, crop_w, crop_h))
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(AnalyzeImagePalette) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_analyze_image_palette() {
        assert_eq!(AnalyzeImagePalette.id(), "analyze_image_palette");
    }

    #[test]
    fn schema_has_image_ref_and_png_params() {
        let s = AnalyzeImagePalette.schema();
        assert_eq!(s.name, "analyze_image_palette");
        assert_eq!(s.parameters["properties"]["image_ref"]["type"], "string");
        assert_eq!(s.parameters["properties"]["png"]["type"], "array");
        assert_eq!(s.parameters["properties"]["crop"]["type"], "object");
    }

    #[test]
    fn schema_does_not_have_rgba_flat() {
        let s = AnalyzeImagePalette.schema();
        // rgba_flat 参数应已删除
        assert!(s.parameters["properties"].get("rgba_flat").is_none());
        assert!(s.parameters["properties"].get("width").is_none());
        assert!(s.parameters["properties"].get("height").is_none());
    }

    #[test]
    fn schema_sensitive_is_true() {
        let s = AnalyzeImagePalette.schema();
        assert!(
            s.sensitive,
            "analyze_image_palette 必须 sensitive=true（分析用户屏幕/剪贴板图片内容属隐私数据）"
        );
    }

    #[test]
    fn danger_class_is_safe() {
        use crate::domain::execution::DangerClass;
        assert_eq!(AnalyzeImagePalette.danger_class(), DangerClass::Safe);
    }

    #[test]
    fn schema_description_mentions_palette() {
        let s = AnalyzeImagePalette.schema();
        assert!(
            s.description.contains("配色") || s.description.contains("palette"),
            "schema description 应提及配色/palette"
        );
    }

    #[test]
    fn capability_is_in_inventory() {
        let registry = crate::domain::capability::CapabilityRegistry::new();
        assert!(
            registry.get("analyze_image_palette").is_some(),
            "analyze_image_palette 应通过 inventory 注册"
        );
    }

    // ── apply_crop 纯函数测试 ─────────────────────────────────────────────

    #[test]
    fn apply_crop_full_image() {
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255];
        let crop = json!({ "x": 0, "y": 0, "w": 2, "h": 2 });
        let (out, w, h) = apply_crop(&rgba, 2, 2, &crop).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, rgba);
    }

    #[test]
    fn apply_crop_top_left_pixel() {
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255];
        let crop = json!({ "x": 0, "y": 0, "w": 1, "h": 1 });
        let (out, w, h) = apply_crop(&rgba, 2, 2, &crop).unwrap();
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, vec![255, 0, 0, 255]);
    }

    #[test]
    fn apply_crop_center() {
        let rgba = vec![
            10, 10, 10, 255, 20, 20, 20, 255, 30, 30, 30, 255, 40, 40, 40, 255,
        ];
        let crop = json!({ "x": 1, "y": 1, "w": 1, "h": 1 });
        let (out, w, h) = apply_crop(&rgba, 2, 2, &crop).unwrap();
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, vec![40, 40, 40, 255]);
    }

    #[test]
    fn apply_crop_clamps_negative_origin() {
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255];
        // x=-1, y=-1 → clamp 到 (0, 0)
        let crop = json!({ "x": -1, "y": -1, "w": 2, "h": 2 });
        let (out, w, h) = apply_crop(&rgba, 2, 2, &crop).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, rgba);
    }

    #[test]
    fn apply_crop_clamps_overflow() {
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255];
        // w=10, h=10 但图片只有 2x2 → clamp 到 2x2
        let crop = json!({ "x": 0, "y": 0, "w": 10, "h": 10 });
        let (out, w, h) = apply_crop(&rgba, 2, 2, &crop).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, rgba);
    }

    #[test]
    fn apply_crop_zero_size_returns_error() {
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255];
        let crop = json!({ "x": 0, "y": 0, "w": 0, "h": 1 });
        let err = apply_crop(&rgba, 2, 2, &crop).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn apply_crop_out_of_bounds_returns_error() {
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255];
        // x=5, y=5 超出 2x2 图片
        let crop = json!({ "x": 5, "y": 5, "w": 1, "h": 1 });
        let err = apply_crop(&rgba, 2, 2, &crop).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn apply_crop_missing_w_returns_error() {
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255];
        let crop = json!({ "x": 0, "y": 0, "h": 1 });
        let err = apply_crop(&rgba, 2, 2, &crop).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    // ── build_palette_items 测试 ──────────────────────────────────────────

    #[test]
    fn build_palette_items_empty_returns_empty() {
        let result = palette::PaletteResult {
            roles: vec![],
            theme: palette::analyze_theme(&[]),
            sample: palette::SampleInfo {
                width: 0,
                height: 0,
                valid_pixels: 0,
                scanned_pixels: 0,
                mode: "full".into(),
            },
            recommended: vec![],
            full: vec![],
            empty: true,
        };
        let items = build_palette_items(&result);
        assert!(items.is_empty());
    }

    #[test]
    fn build_palette_items_non_empty_has_items() {
        let result = palette::PaletteResult {
            roles: vec![palette::RoleColor {
                rgb: [255, 0, 0],
                role: "background".into(),
                ratio: 1.0,
                oklab: [0.5, 0.2, 0.1],
                hex: "#FF0000".into(),
            }],
            theme: palette::analyze_theme(&[]),
            sample: palette::SampleInfo {
                width: 2,
                height: 2,
                valid_pixels: 4,
                scanned_pixels: 4,
                mode: "full".into(),
            },
            recommended: vec![palette::HarmonyScheme {
                label: "图片主题色".into(),
                scheme: "source".into(),
                description: "来自整块选区的聚类原色".into(),
                colors: vec![[255, 0, 0]],
                source_kind: "extraction".into(),
                confidence: 1.0,
            }],
            full: vec![],
            empty: false,
        };
        let items = build_palette_items(&result);
        // 1 role + 1 scheme = 2 items
        assert_eq!(items.len(), 2);

        // 第一项是 role_color
        assert_eq!(items[0].data["type"], "role_color");
        assert_eq!(items[0].data["role"], "background");
        assert_eq!(items[0].data["hex"], "#FF0000");

        // 第二项是 scheme
        assert_eq!(items[1].data["type"], "scheme");
        assert_eq!(items[1].data["source_kind"], "extraction");
        assert_eq!(items[1].data["confidence"], 1.0);
    }

    #[test]
    fn build_palette_items_generated_scheme_has_generated_source_kind() {
        let result = palette::PaletteResult {
            roles: vec![],
            theme: palette::analyze_theme(&[]),
            sample: palette::SampleInfo {
                width: 0,
                height: 0,
                valid_pixels: 0,
                scanned_pixels: 0,
                mode: "full".into(),
            },
            recommended: vec![],
            full: vec![palette::HarmonyScheme {
                label: "同色层级".into(),
                scheme: "generated-tones".into(),
                description: "同一基准色的明暗层级".into(),
                colors: vec![[100, 100, 100]],
                source_kind: "generated".into(),
                confidence: 0.8,
            }],
            empty: false,
        };
        let items = build_palette_items(&result);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].data["source_kind"], "generated");
        assert_eq!(items[0].data["scheme_id"], "generated-tones");
    }

    // ── 资源上限测试 ─────────────────────────────────────────────────────

    #[test]
    fn max_input_bytes_is_32mib() {
        assert_eq!(MAX_INPUT_BYTES, 32 * 1024 * 1024);
    }

    #[test]
    fn max_decoded_pixels_is_256mib_div_4() {
        assert_eq!(MAX_DECODED_PIXELS, (256 * 1024 * 1024) / 4);
    }
}
