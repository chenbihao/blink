//! `capture_screen` Capability（0.9.7 → 0.11.7-f alias）。
//!
//! **0.11.7-f 重构**：核心实现移到 `screenshot.rs`（op=capture）。本文件作为 tool 名
//! alias 保留 3 个月，避免 AI 提示词层缓存失效（详见 phases/0.11.7 §12.6）。
//!
//! **TODO(0.13)** ⏰ 别名到期删除：本文件 + `crop_image.rs` 在 0.13 阶段一起清理。
//! 需同步：
//! 1. 从 `capability/builtins/mod.rs` 的 inventory 移除 `CaptureScreen` / `CropImage`
//! 2. 检查 AI provider 层是否还有 `capture_screen` / `crop_image` 的硬编码引用
//! 3. 更新 `phases/0.12-ai-ecosystem.md` 里 tool 列表快照
//! 4. 保底：搜代码库确认无 test / doc / prompt 残留

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};

/// `capture_screen` — 截取虚拟屏幕，返回 PNG（alias to `screenshot { op: capture }`）。
pub struct CaptureScreen;

#[async_trait::async_trait]
impl Capability for CaptureScreen {
    fn id(&self) -> &str {
        "capture_screen"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "capture_screen".into(),
            description: "截取屏幕（整个虚拟屏幕），返回 PNG 图片。（alias：等价于 screenshot { op: capture }）".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "display": {
                        "type": "integer",
                        "description": "显示器编号（已废弃，使用 screenshot { op: capture, display_id } 替代）",
                        "default": 0
                    }
                }
            }),
            ..Default::default()
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // 委托到统一 screenshot Capability 的 op=capture（无 display_id → 虚拟屏幕）
        super::screenshot::op_capture(None).await
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(CaptureScreen) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_capture_screen() {
        assert_eq!(CaptureScreen.id(), "capture_screen");
    }

    #[test]
    fn schema_marks_alias() {
        let s = CaptureScreen.schema();
        assert_eq!(s.name, "capture_screen");
        assert!(s.description.contains("alias"));
    }

    /// alias 委托：走同一 op_capture 路径。
    #[tokio::test]
    async fn alias_delegates_to_screenshot_op() {
        use crate::infra::platform::screenshot::backend_fake::FakeScreenshotBackend;
        // 与 screenshot::tests 共享全局 backend，串行化
        let _g = super::super::screenshot::test_helpers::test_lock();

        let fake = Arc::new(FakeScreenshotBackend::single_primary(400, 300));
        crate::infra::platform::screenshot::install_backend(fake);
        crate::infra::platform::screenshot::end_session();

        let result = super::super::screenshot::op_capture(None).await.unwrap();
        assert!(matches!(result, CapabilityResult::Blob { .. }));
    }
}
