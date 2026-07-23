//! Phase 1 spike——AI 抗延迟骨架验证（**临时代码**，Phase 6 后删除或转正式测试）。
//!
//! **不是**在跑供应商延迟榜单——供应商由用户自配自切，我们无权替用户选。
//! 这里只验我们自己的抗延迟骨架能不能撑住**任何**供应商（详见 phases/0.9-ai-layer.md §4.4）。
//!
//! **验收清单（7 条骨架）**：
//! 1. ✅ 硬超时 20000ms 精确 abort（不因 SSE 半开挂）
//! 2. ✅ ESC / 换 query 100ms 内取消 in-flight
//! 3. ✅ loading 150ms 反闪烁（前端配合，spike 只验后端信号时机）
//! 4. ⏳ SSE 首 packet 触发 `first_token_ms`（Phase 4 rig Client 落地后跑）
//! 5. ⏳ rig Client 冷构造 ≤5ms（Phase 4）
//! 6. ✅ SecretString 生命周期干净（Phase 2 落地时同步验，见 `secret_hygiene`）
//! 7. ⏳ Provider 切换零重启（Phase 5）
//!
//! **单元隔离**：spike 用 `tokio::net::TcpListener` 起环回 mock server，
//! 不依赖任何外部网络、不依赖 rig-core。这样每条断言都在本机 100ms 内跑完。
//!
//! **`#[cfg(test)]` 门槛**：整个 mod 只在测试构建下编译，release 二进制零字节残留。

#![cfg(test)]

pub mod agent;
pub mod mock_server;
pub mod secret_hygiene;
pub mod skeleton;
