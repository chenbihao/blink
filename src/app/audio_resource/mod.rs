//! AudioResourceRegistry — 有界 audio_ref 资源注册表（0.22.16 H03）。
//!
//! **目的**：负责可信本地路径到 opaque `audio_ref` 的签发与安全解析。
//! 本任务只交付资源生命周期和文件身份安全，不解码、不转写。
//!
//! **设计参考**：复用 `ImageStash` 的 token、TTL、有界淘汰和测试思路，
//! 但不复用 `ImageStash` 保存音频内容——这里只保存文件元数据和受控 reader。
//!
//! **安全模型**：
//! - 签发时仅接受用户通过可信入口显式选择的绝对本地路径
//! - 验证 regular file，记录 canonical path、size、mtime、平台文件 identity
//! - token 不可预测——使用 xorshift + 随机状态混合，不依赖路径/hash 可逆推导
//! - 有 TTL、最大条目数、累计字节预算和惰性清理/淘汰
//! - 不把绝对路径返回给非可信调用方或写入日志
//!
//! **解析时**：
//! - 拒绝不存在、跨 registry、过期、generation 不符和 scope 不符
//! - 拒绝目录、非 regular file 及 symlink/reparse point
//! - 复核 identity、size、mtime；文件被替换或修改后旧 ref 必须失效
//! - 返回已打开的 file handle + identity，避免"校验路径后重新打开"的 TOCTOU
//! - ref 在解析成功后到期，不应中断本次已授权请求
//!
//! **分层**：app 层模块，消费 `infra/platform/file_identity`。
//! 不依赖 Tauri、domain。后续 agent 负责 wiring 到 CapabilityEnv / main.rs。

mod error;
mod registry;

pub use error::{AudioRefError, AudioRefErrorKind};
#[allow(unused_imports)]
pub use registry::{AudioResourceConfig, AudioResourceRegistry, OpenedAudioResource};

#[allow(unused_imports)]
pub use registry::RegistryStats;
