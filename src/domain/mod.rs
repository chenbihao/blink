//! 领域层 re-export shim（0.25.4 crate 化总收纳）。
//!
//! domain 实体分布：
//! - `blink-domain`（本域主体：ai/capability 协议+builtins/chord/config/
//!   context/editor/feature_catalog/intent/mcp/ocr/plugin/resource/search/
//!   sticky + 根公共底座 clipboard/color/event/event_names/palette/schema）
//! - `blink-domain-stt` / `blink-domain-local-engine` / `blink-domain-config`
//!   / `blink-domain-capability`（0.25.1–0.25.3 先行拆出）
//!
//! 本模块保持 `crate::domain::*` 旧路径可用，bin/app/cli 侧引用零改写。

#[allow(unused_imports)] // 门面 re-export：保持 crate::domain::* 全量旧路径（部分模块当前无 bin 引用）
pub use blink_domain::{
    ai, capability, chord, clipboard, color, config, context, editor, event, event_names,
    feature_catalog, intent, mcp, ocr, palette, plugin, resource, schema, search, sticky,
};
pub use blink_domain_local_engine as local_engine;
pub use blink_domain_stt as stt;
