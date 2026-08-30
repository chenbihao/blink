//! 诊断 Capability（0.21.1）——从 Action 迁移。
//!
//! `blink_print_debug_info`：采集 Blink 运行时诊断信息，返回结构化 `Text` 结果。
//! 复制到剪贴板的副作用由调用方（兼容桥/ResultAction）完成，不在此 Capability 中执行。
//! sensitive 诊断 Capability，AI 默认关闭，MCP 默认关但可显式开。
//!
//! `blink_debug_inithook`：采集诊断 + 请求 Hook 恢复。local-only，MCP 禁止。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};

// ── BlinkPrintDebugInfo ─────────────────────────────────────────────────────

pub struct BlinkPrintDebugInfo;

#[async_trait::async_trait]
impl Capability for BlinkPrintDebugInfo {
    fn id(&self) -> &str {
        "blink_print_debug_info"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "blink_print_debug_info".into(),
            description: "Collect general Blink runtime debug info and return as structured text. Currently includes detailed Windows hook and input state diagnostics.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            sensitive: true, // 诊断信息含运行时状态，属敏感数据
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            // local+AI+CLI 可调；MCP 默认关但可显式开
            allowed_origins: OriginSet::LOCAL_AND_CLI | OriginSet::MCP,
            runtime_requirement: RuntimeRequirement::MAIN_PROCESS,
            danger: DangerClass::Safe,
            sensitive: true,
            ai_default: AiDefault::Off,          // 诊断类默认关闭
            mcp_default: McpDefault::DefaultOff, // 默认关，可显式开
            confirmation: ConfirmationPolicy::sensitive(),
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        tracing::info!("执行 Capability：blink_print_debug_info");

        let physical = crate::infra::platform::hotkey::read_physical_modifiers();
        let snapshot =
            crate::infra::platform::hotkey::diagnostics::take_diagnostic_snapshot(physical);
        let events = crate::infra::platform::hotkey::diagnostics::take_diagnostic_events();
        let text = format_diagnostic_info(&snapshot, &events);

        // 返回结构化文本——复制到剪贴板由调用方（兼容桥/ResultAction）完成
        Ok(CapabilityResult::Text {
            content: text,
            desc: Some("Blink 调试信息".into()),
        })
    }
}

// ── BlinkDebugInitHook ──────────────────────────────────────────────────────

pub struct BlinkDebugInitHook;

#[async_trait::async_trait]
impl Capability for BlinkDebugInitHook {
    fn id(&self) -> &str {
        "blink_debug_inithook"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "blink_debug_inithook".into(),
            description: "Collect current Blink debug info, then safely reset volatile Windows input state and request hook reinstallation".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            sensitive: true,
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            // 只允许明确本地入口
            allowed_origins: OriginSet::LOCAL_SURFACE | OriginSet::LOCAL_COMMAND,
            runtime_requirement: RuntimeRequirement::MAIN_PROCESS,
            danger: DangerClass::Safe,
            sensitive: true,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::sensitive(),
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        tracing::info!("执行 Capability：恢复输入钩子（诊断 + ManualRecovery）");

        // 1. 采集诊断快照
        let physical = crate::infra::platform::hotkey::read_physical_modifiers();
        let snapshot =
            crate::infra::platform::hotkey::diagnostics::take_diagnostic_snapshot(physical);
        let events = crate::infra::platform::hotkey::diagnostics::take_diagnostic_events();
        let text = format_diagnostic_info(&snapshot, &events);

        // 2. 请求手动 Hook 恢复
        crate::infra::platform::hotkey::InputController::request_manual_recovery();
        tracing::info!("ManualRecovery 已请求");

        Ok(CapabilityResult::Text {
            content: text,
            desc: Some("Blink 调试信息（含 Hook 恢复请求）".into()),
        })
    }
}

// ── 格式化诊断信息（从 execution/builtin.rs 迁移）──────────────────────────

/// 格式化诊断快照为可读文本。
fn format_diagnostic_info(
    snapshot: &crate::infra::platform::hotkey::diagnostics::InputDiagnosticSnapshot,
    events: &[crate::infra::platform::hotkey::diagnostics::InputDiagnosticEvent],
) -> String {
    let mut lines = Vec::new();

    lines.push("=== Blink Debug Info ===".to_string());
    lines.push("Schema: 1".to_string());
    lines.push("Profile: windows_input".to_string());
    lines.push(format!("Version: {}", env!("CARGO_PKG_VERSION")));
    lines.push(format!(
        "Platform: {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    lines.push(format!("Uptime: {}ms", snapshot.uptime_ms));
    lines.push(String::new());

    // ── Modifiers ──
    lines.push("--- Modifiers (Level | Physical) ---".to_string());
    let key_names = [
        "LCtrl", "RCtrl", "LShift", "RShift", "LAlt", "RAlt", "LMeta", "RMeta",
    ];
    let phys = [
        snapshot.physical.lctrl,
        snapshot.physical.rctrl,
        snapshot.physical.lshift,
        snapshot.physical.rshift,
        snapshot.physical.lalt,
        snapshot.physical.ralt,
        snapshot.physical.lmeta,
        snapshot.physical.rmeta,
    ];
    for (i, name) in key_names.iter().enumerate() {
        let level = level_str(snapshot.state.modifier_levels[i]);
        let p = if phys[i] { "Down" } else { "Up" };
        lines.push(format!("{name:>6}: {level:>12} | {p}"));
    }
    lines.push(format!(
        "Pressed mask: 0x{:04x}",
        snapshot.state.pressed_mask
    ));
    lines.push(String::new());

    // ── Gesture ──
    lines.push("--- Gesture ---".to_string());
    let gesture = if snapshot.state.gesture_idle {
        "Idle"
    } else if snapshot.state.gesture_armed {
        "Armed"
    } else {
        "Active"
    };
    lines.push(format!("State: {gesture}"));
    lines.push(String::new());

    // ── Chord ──
    lines.push("--- Chord ---".to_string());
    lines.push(format!(
        "Active: {}, Session: {:?}",
        snapshot.state.chord_active, snapshot.state.chord_session_id
    ));
    lines.push(String::new());

    // ── Voice / Recorder ──
    lines.push("--- Voice / Recorder ---".to_string());
    lines.push(format!(
        "Voice: {}, Recorder: {}",
        if snapshot.state.voice_idle {
            "Idle"
        } else {
            "Active"
        },
        if snapshot.state.recorder_idle {
            "Idle"
        } else {
            "Active"
        },
    ));
    lines.push(String::new());

    // ── Window ──
    lines.push("--- Window ---".to_string());
    lines.push(format!(
        "Visible: {}, Revision: {}",
        snapshot.state.window_visible, snapshot.state.window_revision
    ));
    lines.push(String::new());

    // ── View ──
    lines.push("--- View ---".to_string());
    lines.push(format!(
        "Ready: {}, Epoch: {}, Revision: {}",
        snapshot.state.view_ready, snapshot.state.view_epoch, snapshot.state.view_revision
    ));
    lines.push(format!(
        "QueryEmpty: {}, AiMode: {}",
        snapshot.state.view_query_empty, snapshot.state.view_ai_mode
    ));
    lines.push(String::new());

    // ── Config ──
    lines.push("--- Config ---".to_string());
    lines.push(format!("Revision: {}", snapshot.state.config_revision));
    lines.push(String::new());

    // ── UI State ──
    lines.push("--- UI State ---".to_string());
    lines.push(format!(
        "Desired:  Alt={} Chord={} Rev={}",
        snapshot.state.desired_alt_down,
        snapshot.state.desired_chord_active,
        snapshot.state.desired_revision
    ));
    lines.push(format!(
        "Published: Alt={} Chord={} Rev={}",
        snapshot.published_alt_down, snapshot.published_chord_active, snapshot.published_revision
    ));
    lines.push(String::new());

    // ── Hook ──
    lines.push("--- Hook ---".to_string());
    lines.push(format!(
        "Installed: {}, Available: {}",
        snapshot.hook.hook_installed, snapshot.hook.hook_available
    ));
    lines.push(format!(
        "PendingReinstall: {:?}, Attempt: {}",
        snapshot.hook.pending_reinstall, snapshot.hook.reinstall_attempt
    ));
    lines.push(format!(
        "WTS: {}, Raw: {}",
        snapshot.hook.wts_registered, snapshot.hook.raw_registered
    ));
    lines.push(format!(
        "Hook generation: {}",
        snapshot.hook.hook_generation
    ));
    lines.push(String::new());

    // ── Recent Events ──
    lines.push(format!("--- Recent Events ({}) ---", events.len()));
    for event in events.iter().rev().take(20) {
        lines.push(format_event(event));
    }
    lines.push(String::new());

    // ── Findings ──
    lines.push("--- Findings ---".to_string());
    let mut findings = Vec::new();
    for (i, name) in key_names.iter().enumerate() {
        let cached_down = snapshot.state.modifier_levels[i].is_pressed();
        if cached_down != phys[i] {
            findings.push(format!(
                "ERROR MODIFIER_MISMATCH: {name} cached={} physical={}",
                if cached_down { "Down" } else { "Up" },
                if phys[i] { "Down" } else { "Up" }
            ));
        }
    }
    if snapshot.state.desired_revision != snapshot.published_revision
        || snapshot.state.desired_alt_down != snapshot.published_alt_down
        || snapshot.state.desired_chord_active != snapshot.published_chord_active
    {
        findings.push(format!(
            "ERROR UI_PROJECTION_MISMATCH: desired_rev={} published_rev={}",
            snapshot.state.desired_revision, snapshot.published_revision
        ));
    }
    if !snapshot.hook.hook_installed || !snapshot.hook.hook_available {
        findings.push("ERROR HOOK_UNAVAILABLE".to_string());
    }
    if findings.is_empty() {
        lines.push("OK: no known input inconsistency detected".to_string());
    } else {
        lines.extend(findings);
    }

    lines.join("\n")
}

/// 格式化 ModifierLevel 为短字符串。
fn level_str(level: crate::infra::platform::hotkey::ModifierLevel) -> &'static str {
    use crate::infra::platform::hotkey::ModifierLevel;
    match level {
        ModifierLevel::Unknown => "Unknown",
        ModifierLevel::Up => "Up",
        ModifierLevel::Down => "Down",
        ModifierLevel::InjectedDown => "InjectedDn",
        ModifierLevel::InferredDown => "InferredDn",
    }
}

/// 格式化单条诊断事件。
fn format_event(
    event: &crate::infra::platform::hotkey::diagnostics::InputDiagnosticEvent,
) -> String {
    use crate::infra::platform::hotkey::diagnostics::{
        DiagnosticKeyClass, DiagnosticSource, DiagnosticTransition,
    };

    let src = match event.source {
        DiagnosticSource::Hook => "Hook",
        DiagnosticSource::Raw => "Raw",
        DiagnosticSource::Physical => "Phys",
        DiagnosticSource::Control => "Ctrl",
        DiagnosticSource::SessionReset => "SReset",
        DiagnosticSource::HoldTimer => "Timer",
    };
    let key = match event.key {
        DiagnosticKeyClass::Modifier(m) => match m {
            crate::infra::platform::hotkey::ModifierKey::LCtrl => "LCtrl",
            crate::infra::platform::hotkey::ModifierKey::RCtrl => "RCtrl",
            crate::infra::platform::hotkey::ModifierKey::LShift => "LShift",
            crate::infra::platform::hotkey::ModifierKey::RShift => "RShift",
            crate::infra::platform::hotkey::ModifierKey::LAlt => "LAlt",
            crate::infra::platform::hotkey::ModifierKey::RAlt => "RAlt",
            crate::infra::platform::hotkey::ModifierKey::LMeta => "LMeta",
            crate::infra::platform::hotkey::ModifierKey::RMeta => "RMeta",
        },
        DiagnosticKeyClass::MainKey => "MainKey",
        DiagnosticKeyClass::OtherKey => "OtherKey",
        DiagnosticKeyClass::None => "-",
    };
    let trans = match event.transition {
        DiagnosticTransition::Down => "Down",
        DiagnosticTransition::Up => "Up",
        DiagnosticTransition::Reconcile => "Reconcile",
        DiagnosticTransition::ConfigChanged => "ConfigChg",
        DiagnosticTransition::WindowChanged => "WindowChg",
        DiagnosticTransition::VoicePhaseChanged => "VoiceChg",
        DiagnosticTransition::RecorderModeChanged => "RecorderChg",
        DiagnosticTransition::SessionReset => "SessionReset",
        DiagnosticTransition::ManualRecovery => "ManualRecovery",
        DiagnosticTransition::HoldDeadline => "HoldDeadline",
        DiagnosticTransition::RawDeviceRemoved => "DevRemoved",
    };
    let inj = match event.injected {
        Some(true) => " inj=T",
        Some(false) => " inj=F",
        None => "",
    };
    let chord = format!("{}→{}", event.chord_before, event.chord_after);
    let level = match (event.before_level, event.after_level) {
        (Some(before), Some(after)) => {
            format!(" level:{}→{}", level_str(before), level_str(after))
        }
        _ => String::new(),
    };

    format!(
        "[{:04}] +{}ms {}/{} {}{}{} chord:{}",
        event.seq, event.elapsed_ms, src, key, trans, inj, level, chord
    )
}

// ── inventory 注册 ──────────────────────────────────────────────────────────

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(BlinkPrintDebugInfo) as Arc<dyn Capability>,
});
inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(BlinkDebugInitHook) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blink_print_debug_info_is_sensitive_ai_off() {
        let p = BlinkPrintDebugInfo.policy();
        assert_eq!(p.danger, DangerClass::Safe);
        assert!(p.sensitive);
        assert_eq!(p.ai_default, AiDefault::Off);
        assert_eq!(p.mcp_default, McpDefault::DefaultOff); // 可显式开
    }

    #[test]
    fn blink_debug_inithook_is_local_only() {
        let p = BlinkDebugInitHook.policy();
        assert!(p.sensitive);
        assert!(!p.allows_origin(crate::domain::capability::InvocationOrigin::LocalAi));
        assert!(!p.allows_origin(crate::domain::capability::InvocationOrigin::Mcp));
        assert_eq!(p.mcp_default, McpDefault::Forbidden);
    }

    #[test]
    fn both_have_non_empty_schema_description() {
        let s = BlinkPrintDebugInfo.schema();
        assert!(!s.description.is_empty());
        let s = BlinkDebugInitHook.schema();
        assert!(!s.description.is_empty());
    }
}
